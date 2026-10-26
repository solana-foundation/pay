//! `pay fanout` — high-throughput, journaled CSV stablecoin payouts.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use chrono::Utc;
use ed25519_dalek::SigningKey;
use futures_util::{StreamExt, TryStreamExt};
use pay_core::accounts::{
    Account, AccountChoice, AccountsFile, AccountsStore, BackendKind, FileAccountsStore,
    MemoryAccountsStore, resolve_account_for_network,
};
use pay_core::client::push::executor::{
    ChunkTxContext, DirectSolanaBroadcaster, PushExecutor, PushExecutorConfig,
    drop_permit_off_runtime_thread,
};
use pay_core::client::push::journal::{
    ChunkResumeAction, Journal, JournalEvent, JournalEventKind, Landing, RecordedPayout, ResumeRpc,
    default_journal_path, reduce_chunk_resume_action,
};
use pay_core::client::push::manifest::{ManifestContext, TransferManifest, parse_manifest_csv};
use pay_core::client::push::permit::{BatchAuthorizationSummary, BatchSigningPermit};
use pay_core::client::push::planner::{
    AtaSnapshot, DestinationAtaStatus, FeePayerMode, compute_reserve_lamports, pack_chunks_from,
    token_account_len,
};
use pay_core::{Error, Result};
use pay_kit::mpp::protocol::solana::{default_rpc_url, programs};
use pay_types::landing::{ChainState, ChainTip, SignatureStatuses, signature_status_from_rpc};
use pay_types::{Stablecoin, stablecoin_mints};
use serde_json::{Value, json};
use solana_pubkey::Pubkey;

const DEFAULT_MAX_IN_FLIGHT: usize = 128;
const DEFAULT_POLL_INTERVAL_MS: u64 = 250;
const PREFLIGHT_CONCURRENCY: usize = 32;
const ESTIMATED_FEE_LAMPORTS_PER_CHUNK: u64 = 10_000;

#[derive(clap::Args)]
pub struct FanoutCommand {
    /// RFC 4180 CSV with exactly `recipient,amount` headers.
    #[arg(value_name = "CSV")]
    csv: PathBuf,

    /// Stablecoin symbol (for example USDC, USDG, or devnet-only USDtest).
    #[arg(long, value_name = "STABLECOIN")]
    currency: String,

    /// Solana network whose mint and account registry entry will be used.
    #[arg(long, default_value = "mainnet")]
    network: String,

    /// Explicit Solana JSON-RPC URL. Defaults to PAY_RPC_URL, then the network default.
    #[arg(long, value_name = "URL")]
    rpc_url: Option<String>,

    /// Solana CLI JSON keypair. Allowed only off mainnet; it is never persisted.
    #[arg(long, value_name = "PATH")]
    keypair: Option<PathBuf>,

    /// Maximum broadcast transactions awaiting confirmation at once.
    #[arg(long, default_value_t = DEFAULT_MAX_IN_FLIGHT)]
    max_in_flight: usize,

    /// Durable JSONL journal path. Defaults under ~/.config/pay/push/.
    #[arg(long, value_name = "PATH", conflicts_with = "resume")]
    journal: Option<PathBuf>,

    /// Resume the run recorded in this journal: confirmed rows are skipped
    /// and only rows proven unpaid are signed again.
    #[arg(long, value_name = "JOURNAL")]
    resume: Option<PathBuf>,

    /// Start an independent payout even though a journal for the same CSV,
    /// account, and network already exists.
    #[arg(long, conflicts_with = "resume")]
    new_run: bool,
}

impl FanoutCommand {
    pub fn run(
        self,
        network_override: Option<&str>,
        account_override: Option<&str>,
        verbose: bool,
    ) -> Result<()> {
        let network = effective_network(&self.network, network_override).to_string();
        validate_network(&network)?;
        if network == "mainnet" && self.keypair.is_some() {
            return Err(Error::Config(
                "--keypair is disabled for mainnet fanout; use a configured pay account so its authentication policy is enforced"
                    .to_string(),
            ));
        }
        if self.max_in_flight == 0 {
            return Err(Error::Config(
                "--max-in-flight must be greater than 0".to_string(),
            ));
        }

        let rpc_url = self
            .rpc_url
            .clone()
            .or_else(|| std::env::var("PAY_RPC_URL").ok())
            .unwrap_or_else(|| default_rpc_url(&network).to_string());
        let (currency, mint) = resolve_currency(&self.currency, &network)?;
        let (store, account_name, sender) =
            resolve_signing_account(&network, account_override, self.keypair.as_deref())?;
        let csv = std::fs::read(&self.csv).map_err(|error| {
            Error::Config(format!("failed to read {}: {error}", self.csv.display()))
        })?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| Error::Config(format!("failed to start fanout runtime: {error}")))?;
        let rpc = FanoutRpc::new(rpc_url.clone());
        let mint_info = runtime.block_on(rpc.mint_info(&mint))?;
        let genesis_hash = runtime.block_on(rpc.genesis_hash())?;
        let context = ManifestContext {
            network_genesis_hash: genesis_hash,
            mint,
            token_program: mint_info.token_program,
            decimals: mint_info.decimals,
        };
        let full_manifest = parse_manifest_csv(csv.as_slice(), context)?;
        let manifest_hash = full_manifest.hash_hex();
        let (mut journal, manifest, chunk_offset) = match self.resume.as_deref() {
            Some(path) => {
                let (mut journal, events) = Journal::open_existing(path.to_path_buf())?;
                verify_resume_journal(
                    &events,
                    &full_manifest,
                    &network,
                    &mint,
                    &mint_info.token_program,
                    &sender,
                )?;
                let statuses = runtime.block_on(resume_landings(&rpc, &events))?;
                let resume = plan_resume(&events, &full_manifest, &statuses)?;
                for (chunk_index, signature) in &resume.newly_confirmed {
                    journal.append_chunk_confirmed(*chunk_index, signature)?;
                }
                if resume.remaining.rows.is_empty() {
                    journal.append_run_completed(resume.confirmed_chunks, 0, 0)?;
                    println!(
                        "Fanout already confirmed every row\nJournal: {}",
                        journal.path().display()
                    );
                    return Ok(());
                }
                (journal, resume.remaining, resume.next_chunk_index)
            }
            None => {
                let journal_path = self.journal.clone().unwrap_or_else(|| {
                    default_journal_path(pay_core::client::push::manifest_hash_prefix(
                        &manifest_hash,
                    ))
                });
                if !self.new_run {
                    let mut dirs = vec![
                        default_journal_path("")
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_default(),
                    ];
                    dirs.extend(journal_path.parent().map(Path::to_path_buf));
                    dirs.sort();
                    dirs.dedup();
                    refuse_existing_runs(&dirs, &manifest_hash, &network, &mint, &sender)?;
                }
                let mut journal = Journal::create_new(journal_path)?;
                journal.append_run_created(
                    manifest_hash.clone(),
                    network.clone(),
                    currency.clone(),
                    mint.to_string(),
                    mint_info.token_program.to_string(),
                    mint_info.decimals,
                    full_manifest.rows.len(),
                    full_manifest.total_amount_raw,
                    "self_funded".to_string(),
                )?;
                (journal, full_manifest, 0)
            }
        };
        let journal_path = journal.path().to_path_buf();
        let snapshot =
            runtime.block_on(rpc.ata_snapshot(&sender, &manifest, mint_info.token_program))?;
        let plan = pack_chunks_from(
            &manifest,
            &snapshot,
            FeePayerMode::SelfFunded,
            &sender,
            &sender,
            0,
            chunk_offset,
        )?;

        let source_balance = runtime.block_on(rpc.token_balance(&snapshot.sender_ata))?;
        if source_balance < manifest.total_amount_raw {
            return Err(Error::Config(format!(
                "source account has {source_balance} base units of {currency}; fanout needs {}",
                manifest.total_amount_raw
            )));
        }
        let missing_ata_rent_lamports = runtime.block_on(
            rpc.missing_ata_rent_lamports(&mint_info.token_program, snapshot.missing_count()),
        )?;
        let estimated_fee_lamports = ESTIMATED_FEE_LAMPORTS_PER_CHUNK
            .checked_mul(plan.total_transactions() as u64)
            .ok_or_else(|| Error::Config("transaction fee estimate overflowed u64".to_string()))?;
        let reserve_lamports = compute_reserve_lamports(estimated_fee_lamports);
        let needed_sol = missing_ata_rent_lamports
            .checked_add(estimated_fee_lamports)
            .and_then(|value| value.checked_add(reserve_lamports))
            .ok_or_else(|| Error::Config("fanout SOL estimate overflowed u64".to_string()))?;
        let available_sol = runtime.block_on(rpc.sol_balance(&sender))?;
        if available_sol < needed_sol {
            return Err(Error::Config(format!(
                "fee payer {sender} has {available_sol} lamports; fanout needs about {needed_sol} for {} missing ATAs, fees, and reserve",
                snapshot.missing_count()
            )));
        }

        journal.append_preflight_completed(
            FeePayerMode::SelfFunded,
            estimated_fee_lamports,
            missing_ata_rent_lamports,
            reserve_lamports,
            plan.total_transactions(),
            manifest.total_amount_raw,
        )?;

        if verbose {
            eprintln!(
                "Fanout plan: {} recipients, {} transactions, {} missing ATAs, {} max in flight",
                manifest.rows.len(),
                plan.total_transactions(),
                snapshot.missing_count(),
                self.max_in_flight
            );
        }

        let summary = BatchAuthorizationSummary {
            account: &account_name,
            currency: &currency,
            currency_decimals: mint_info.decimals,
            network: &network,
            recipient_total_raw: manifest.total_amount_raw,
            max_total_raw: manifest.total_amount_raw,
        };
        let mut permit = BatchSigningPermit::authorize(
            &network,
            store.as_ref(),
            Some(&account_name),
            genesis_hash,
            &manifest,
            plan.clone(),
            summary,
            chrono::Duration::hours(6),
            None,
        )?;
        journal.append_authorization_granted(
            sender.to_string(),
            manifest.total_amount_raw,
            plan.total_transactions(),
            permit.expires_at(),
        )?;

        let broadcaster = DirectSolanaBroadcaster::new(rpc_url, sender);
        let executor_config = PushExecutorConfig {
            max_in_flight: self.max_in_flight,
            poll_interval: Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
        };
        let outcome = runtime.block_on(async {
            let mut executor = PushExecutor::new(
                &mut permit,
                &mut journal,
                &broadcaster,
                ChunkTxContext {
                    sender,
                    mint,
                    token_program: mint_info.token_program,
                    decimals: mint_info.decimals,
                },
                executor_config,
            );
            executor.run(&plan.chunks).await
        });
        drop_permit_off_runtime_thread(permit);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = journal.append_run_interrupted(
                    error.to_string(),
                    0,
                    0,
                    plan.total_transactions(),
                );
                return Err(Error::Config(format!(
                    "fanout interrupted: {error}; resume with --resume {}",
                    journal_path.display()
                )));
            }
        };
        journal.append_run_completed(outcome.confirmed, outcome.failed, 0)?;
        if outcome.failed > 0 {
            return Err(Error::Config(format!(
                "fanout finished with {} confirmed and {} failed chunks; resume with --resume {}",
                outcome.confirmed,
                outcome.failed,
                journal_path.display()
            )));
        }

        println!(
            "Fanout confirmed: {} recipients in {} transactions\nJournal: {}",
            manifest.rows.len(),
            outcome.confirmed,
            journal_path.display()
        );
        Ok(())
    }
}

#[derive(Clone)]
struct FanoutRpc {
    http: reqwest::Client,
    url: String,
}

#[derive(Clone, Copy)]
struct MintInfo {
    token_program: Pubkey,
    decimals: u8,
}

impl FanoutRpc {
    fn new(url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            url,
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let response = self
            .http
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .map_err(|error| Error::Config(format!("{method} transport failed: {error}")))?;
        let body: Value = response
            .json()
            .await
            .map_err(|error| Error::Config(format!("{method} returned invalid JSON: {error}")))?;
        if let Some(error) = body.get("error") {
            return Err(Error::Config(format!("{method} RPC error: {error}")));
        }
        body.get("result")
            .cloned()
            .ok_or_else(|| Error::Config(format!("{method} response is missing result")))
    }

    async fn genesis_hash(&self) -> Result<[u8; 32]> {
        let hash = self
            .call("getGenesisHash", json!([]))
            .await?
            .as_str()
            .ok_or_else(|| Error::Config("malformed getGenesisHash response".to_string()))?
            .to_string();
        let bytes = bs58::decode(hash)
            .into_vec()
            .map_err(|error| Error::Config(format!("invalid genesis hash: {error}")))?;
        bytes
            .try_into()
            .map_err(|_| Error::Config("genesis hash must decode to 32 bytes".to_string()))
    }

    async fn mint_info(&self, mint: &Pubkey) -> Result<MintInfo> {
        let result = self
            .call(
                "getAccountInfo",
                json!([mint.to_string(), {"encoding":"jsonParsed","commitment":"confirmed"}]),
            )
            .await?;
        let value = result
            .get("value")
            .filter(|value| !value.is_null())
            .ok_or_else(|| Error::Config(format!("mint {mint} does not exist")))?;
        let token_program = value
            .get("owner")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Config("mint account owner is missing".to_string()))?
            .parse()
            .map_err(|error| Error::Config(format!("mint token program is invalid: {error}")))?;
        let decimals = value
            .pointer("/data/parsed/info/decimals")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(|| Error::Config("mint decimals are missing or invalid".to_string()))?;
        Ok(MintInfo {
            token_program,
            decimals,
        })
    }

    async fn ata_snapshot(
        &self,
        sender: &Pubkey,
        manifest: &pay_core::client::push::manifest::TransferManifest,
        token_program: Pubkey,
    ) -> Result<AtaSnapshot> {
        let sender_ata = associated_token_address(sender, &manifest.context.mint, &token_program)?;
        let indexed = manifest
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let ata = associated_token_address(
                    &row.recipient,
                    &manifest.context.mint,
                    &token_program,
                )?;
                Ok((index, row.recipient, ata))
            })
            .collect::<Result<Vec<_>>>()?;
        let batches = indexed
            .chunks(100)
            .map(|chunk| chunk.to_vec())
            .collect::<Vec<_>>();
        let mut statuses = futures_util::stream::iter(batches)
            .map(|batch| {
                let rpc = self.clone();
                async move {
                    let addresses = batch
                        .iter()
                        .map(|(_, _, ata)| ata.to_string())
                        .collect::<Vec<_>>();
                    let result = rpc
                        .call(
                            "getMultipleAccounts",
                            json!([addresses, {"encoding":"base64","commitment":"confirmed"}]),
                        )
                        .await?;
                    let values =
                        result
                            .get("value")
                            .and_then(Value::as_array)
                            .ok_or_else(|| {
                                Error::Config("malformed getMultipleAccounts response".to_string())
                            })?;
                    if values.len() != batch.len() {
                        return Err(Error::Config(
                            "getMultipleAccounts returned the wrong number of accounts".to_string(),
                        ));
                    }
                    Ok::<_, Error>(
                        batch
                            .into_iter()
                            .zip(values)
                            .map(|((index, recipient, ata), value)| {
                                (
                                    index,
                                    DestinationAtaStatus {
                                        recipient,
                                        ata,
                                        exists: !value.is_null(),
                                    },
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                }
            })
            .buffer_unordered(PREFLIGHT_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        statuses.sort_by_key(|(index, _)| *index);
        let destinations = statuses.into_iter().map(|(_, status)| status).collect();
        let sender_ata_exists = self.account_exists(&sender_ata).await?;
        Ok(AtaSnapshot {
            sender_ata,
            sender_ata_exists,
            destinations,
        })
    }

    async fn account_exists(&self, address: &Pubkey) -> Result<bool> {
        let result = self
            .call(
                "getAccountInfo",
                json!([address.to_string(), {"encoding":"base64","commitment":"confirmed"}]),
            )
            .await?;
        Ok(result.get("value").is_some_and(|value| !value.is_null()))
    }

    async fn token_balance(&self, address: &Pubkey) -> Result<u64> {
        let result = self
            .call(
                "getTokenAccountBalance",
                json!([address.to_string(), {"commitment":"confirmed"}]),
            )
            .await?;
        result
            .pointer("/value/amount")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Config(format!("token account {address} has no raw balance")))?
            .parse()
            .map_err(|error| Error::Config(format!("token balance is invalid: {error}")))
    }

    async fn missing_ata_rent_lamports(
        &self,
        token_program: &Pubkey,
        missing: usize,
    ) -> Result<u64> {
        if missing == 0 {
            return Ok(0);
        }
        let rent_per_ata = self
            .call(
                "getMinimumBalanceForRentExemption",
                json!([token_account_len(token_program), {"commitment":"confirmed"}]),
            )
            .await?
            .as_u64()
            .ok_or_else(|| {
                Error::Config("malformed getMinimumBalanceForRentExemption response".to_string())
            })?;
        rent_per_ata
            .checked_mul(missing as u64)
            .ok_or_else(|| Error::Config("missing-ATA rent estimate overflowed u64".to_string()))
    }

    async fn sol_balance(&self, address: &Pubkey) -> Result<u64> {
        let result = self
            .call(
                "getBalance",
                json!([address.to_string(), {"commitment":"confirmed"}]),
            )
            .await?;
        result
            .get("value")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Config("malformed getBalance response".to_string()))
    }
}

impl ChainState for FanoutRpc {
    type Error = Error;

    async fn finalized_tip(&self) -> Result<ChainTip> {
        let info = self
            .call("getEpochInfo", json!([{"commitment": "finalized"}]))
            .await?;
        match (
            info.get("absoluteSlot").and_then(Value::as_u64),
            info.get("blockHeight").and_then(Value::as_u64),
        ) {
            (Some(slot), Some(block_height)) => Ok(ChainTip { slot, block_height }),
            _ => Err(Error::Config("malformed getEpochInfo response".to_string())),
        }
    }

    async fn signature_statuses(&self, signatures: &[String]) -> Result<SignatureStatuses> {
        let mut context_slot = u64::MAX;
        let mut statuses = Vec::with_capacity(signatures.len());
        for batch in signatures.chunks(256) {
            let result = self
                .call(
                    "getSignatureStatuses",
                    json!([batch, {"searchTransactionHistory": true}]),
                )
                .await?;
            let malformed = || Error::Config("malformed getSignatureStatuses response".to_string());
            let slot = result
                .pointer("/context/slot")
                .and_then(Value::as_u64)
                .ok_or_else(malformed)?;
            let values = result
                .get("value")
                .and_then(Value::as_array)
                .filter(|values| values.len() == batch.len())
                .ok_or_else(malformed)?;
            context_slot = context_slot.min(slot);
            statuses.extend(values.iter().map(signature_status_from_rpc));
        }
        Ok(SignatureStatuses {
            context_slot,
            statuses,
        })
    }
}

/// Where every payout the journal signed stands, established once for a
/// resume and served to the pure journal reducer.
async fn resume_landings<C: ChainState<Error = Error>>(
    chain: &C,
    events: &[JournalEvent],
) -> Result<ResumeStatuses> {
    let payouts = signed_payouts(events);
    let landings = pay_types::landing::confirm_not_landed(chain, &payouts).await?;
    Ok(ResumeStatuses {
        landings: payouts
            .into_iter()
            .map(|payout| payout.signature)
            .zip(landings)
            .collect(),
    })
}

/// A payout the lookup did not cover is unknown.
struct ResumeStatuses {
    landings: HashMap<String, Landing>,
}

impl ResumeRpc for ResumeStatuses {
    fn landing(&self, payout: &RecordedPayout) -> Landing {
        self.landings
            .get(&payout.signature)
            .cloned()
            .unwrap_or(Landing::Unknown)
    }
}

/// The journal's chunks that never confirmed, keyed by index, with the
/// rows each one carried.
fn journal_chunks(events: &[JournalEvent]) -> BTreeMap<u32, Vec<u64>> {
    let mut chunks = BTreeMap::new();
    for event in events {
        match &event.kind {
            JournalEventKind::ChunkPrepared {
                chunk_index,
                row_numbers,
                ..
            }
            | JournalEventKind::ChunkSigned {
                chunk_index,
                row_numbers,
                ..
            } => {
                chunks.insert(*chunk_index, row_numbers.clone());
            }
            _ => {}
        }
    }
    chunks
}

fn signed_payouts(events: &[JournalEvent]) -> Vec<RecordedPayout> {
    let mut payouts = BTreeMap::new();
    for event in events {
        if let JournalEventKind::ChunkSigned {
            signature,
            last_valid_block_height,
            ..
        } = &event.kind
        {
            payouts.insert(signature.clone(), *last_valid_block_height);
        }
    }
    payouts
        .into_iter()
        .map(|(signature, last_valid)| RecordedPayout {
            signature,
            last_valid_block_height: Some(last_valid),
        })
        .collect()
}

/// A resume may only continue the exact payout the journal recorded.
fn verify_resume_journal(
    events: &[JournalEvent],
    manifest: &TransferManifest,
    network: &str,
    mint: &Pubkey,
    token_program: &Pubkey,
    sender: &Pubkey,
) -> Result<()> {
    let created = events.iter().find_map(|event| match &event.kind {
        JournalEventKind::RunCreated {
            manifest_hash_hex,
            network,
            mint,
            token_program,
            decimals,
            row_count,
            total_amount_raw,
            requested_fee_mode,
            ..
        } => Some((
            manifest_hash_hex,
            network,
            mint,
            token_program,
            *decimals,
            *row_count,
            *total_amount_raw,
            requested_fee_mode,
        )),
        _ => None,
    });
    let Some((hash, journal_network, journal_mint, journal_program, decimals, rows, total, mode)) =
        created
    else {
        return Err(Error::Config(
            "journal has no run_created record; it cannot be resumed".to_string(),
        ));
    };
    if *hash != manifest.hash_hex()
        || journal_network != network
        || *journal_mint != mint.to_string()
        || *journal_program != token_program.to_string()
        || decimals != manifest.context.decimals
        || rows != manifest.rows.len()
        || total != manifest.total_amount_raw
        || mode != "self_funded"
    {
        return Err(Error::Config(
            "journal records a different payout (CSV, network, mint, or fee mode); refusing to resume"
                .to_string(),
        ));
    }
    let signers = events
        .iter()
        .filter_map(|event| match &event.kind {
            JournalEventKind::AuthorizationGranted { account_pubkey, .. } => Some(account_pubkey),
            _ => None,
        })
        .collect::<Vec<_>>();
    if signers
        .iter()
        .any(|account| **account != sender.to_string())
    {
        return Err(Error::Config(
            "journal was authorized by a different account; refusing to resume".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct ResumePlan {
    /// Rows not yet proven paid, under the original manifest identity.
    remaining: TransferManifest,
    /// Chunks RPC proves confirmed that the journal had not yet recorded.
    newly_confirmed: Vec<(u32, solana_signature::Signature)>,
    confirmed_chunks: usize,
    next_chunk_index: u32,
}

/// Decide, chunk by chunk, which rows are paid. A chunk whose signed bytes
/// could still land, or whose status is unknown, stops the resume: signing a
/// replacement then could pay its rows twice.
fn plan_resume(
    events: &[JournalEvent],
    manifest: &TransferManifest,
    rpc: &dyn ResumeRpc,
) -> Result<ResumePlan> {
    let chunks = journal_chunks(events);
    let mut paid = BTreeSet::new();
    let mut newly_confirmed = Vec::new();
    let mut confirmed_chunks = 0;
    for (chunk_index, rows) in &chunks {
        match reduce_chunk_resume_action(*chunk_index, events, FeePayerMode::SelfFunded, rpc) {
            ChunkResumeAction::Skip => {}
            ChunkResumeAction::RecordConfirmedThenSkip { signature } => {
                let signature =
                    solana_signature::Signature::from_str(&signature).map_err(|_| {
                        Error::Config(format!(
                            "journal chunk {chunk_index} has a malformed signature"
                        ))
                    })?;
                newly_confirmed.push((*chunk_index, signature));
            }
            ChunkResumeAction::RetryAfterFailure { .. }
            | ChunkResumeAction::RebuildWithFreshBlockhash { .. }
            | ChunkResumeAction::NeedsSigning => continue,
            ChunkResumeAction::RebroadcastIdenticalBytes { .. } => {
                return Err(Error::Config(format!(
                    "chunk {chunk_index} was signed and may still land; retry --resume once its \
                     blockhash expires at finalized commitment (about two minutes)"
                )));
            }
            ChunkResumeAction::StopUnknown { signature } => {
                return Err(Error::Config(format!(
                    "chunk {chunk_index} signature {signature} has an unknown status; retry \
                     --resume once RPC reports it"
                )));
            }
            ChunkResumeAction::RepresentGaslessCredential { .. } => {
                return Err(Error::Config(
                    "gasless journals cannot be resumed by fanout".to_string(),
                ));
            }
        }
        confirmed_chunks += 1;
        paid.extend(rows.iter().copied());
    }
    let rows = manifest
        .rows
        .iter()
        .filter(|row| !paid.contains(&row.row_number))
        .cloned()
        .collect::<Vec<_>>();
    let total_amount_raw = rows.iter().try_fold(0u64, |total, row| {
        total
            .checked_add(row.amount_raw)
            .ok_or_else(|| Error::Config("remaining fanout amount overflowed u64".to_string()))
    })?;
    let next_chunk_index = match chunks.keys().next_back() {
        Some(last) => last
            .checked_add(1)
            .ok_or_else(|| Error::Config("journal chunk index overflowed u32".to_string()))?,
        None => 0,
    };
    Ok(ResumePlan {
        remaining: TransferManifest {
            context: manifest.context.clone(),
            rows,
            total_amount_raw,
            hash: manifest.hash,
        },
        newly_confirmed,
        confirmed_chunks,
        next_chunk_index,
    })
}

/// Read a journal without the writer's truncation side effect: another
/// process may still own it.
fn read_journal_events(path: &Path) -> Vec<JournalEvent> {
    std::fs::read_to_string(path)
        .map(|raw| {
            raw.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `events` record a run of this CSV by this account on this
/// network that signed at least one chunk, and if so whether it finished.
fn matching_run(
    events: &[JournalEvent],
    manifest_hash: &str,
    network: &str,
    mint: &Pubkey,
    sender: &Pubkey,
) -> Option<bool> {
    let same_payout = events.iter().any(|event| {
        matches!(&event.kind, JournalEventKind::RunCreated {
            manifest_hash_hex, network: n, mint: m, ..
        } if manifest_hash_hex == manifest_hash && n == network && *m == mint.to_string())
    });
    let same_signer = events.iter().any(|event| {
        matches!(&event.kind, JournalEventKind::AuthorizationGranted { account_pubkey, .. }
            if *account_pubkey == sender.to_string())
    });
    let signed = events
        .iter()
        .any(|event| matches!(event.kind, JournalEventKind::ChunkSigned { .. }));
    if !(same_payout && same_signer && signed) {
        return None;
    }
    let finished = matches!(
        events.iter().rev().find(|event| matches!(
            event.kind,
            JournalEventKind::ChunkSigned { .. }
                | JournalEventKind::RunCompleted { .. }
                | JournalEventKind::RunInterrupted { .. }
        )),
        Some(JournalEvent {
            kind: JournalEventKind::RunCompleted {
                failed: 0,
                unknown: 0,
                ..
            },
            ..
        })
    );
    Some(finished)
}

/// A rerun of the same CSV must resume, not start over: a fresh plan would
/// sign every row again, including rows a previous run already paid.
fn refuse_existing_runs(
    dirs: &[PathBuf],
    manifest_hash: &str,
    network: &str,
    mint: &Pubkey,
    sender: &Pubkey,
) -> Result<()> {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let events = read_journal_events(&path);
            match matching_run(&events, manifest_hash, network, mint, sender) {
                Some(false) => {
                    return Err(Error::Config(format!(
                        "an unfinished fanout of this CSV from this account exists; resume it with \
                         --resume {} (or pass --new-run to pay every row again)",
                        path.display()
                    )));
                }
                Some(true) => {
                    return Err(Error::Config(format!(
                        "this CSV was already paid from this account (journal {}); pass --new-run \
                         to pay every row again",
                        path.display()
                    )));
                }
                None => {}
            }
        }
    }
    Ok(())
}

fn resolve_currency(currency: &str, network: &str) -> Result<(String, Pubkey)> {
    if currency.eq_ignore_ascii_case("USDtest") {
        if network != "devnet" {
            return Err(Error::Config(
                "USDtest is devnet-only; pass --network devnet".to_string(),
            ));
        }
        return Ok((
            "USDtest".to_string(),
            Pubkey::from_str(stablecoin_mints::USDTEST_DEVNET)
                .map_err(|error| Error::Config(format!("invalid USDtest mint: {error}")))?,
        ));
    }
    let stablecoin = Stablecoin::from_str(currency).map_err(Error::Config)?;
    Pubkey::from_str(stablecoin.mint(Some(network)))
        .map(|mint| (stablecoin.symbol().to_string(), mint))
        .map_err(|error| Error::Config(format!("invalid {} mint: {error}", stablecoin.symbol())))
}

fn validate_network(network: &str) -> Result<()> {
    match network {
        "mainnet" | "devnet" | "testnet" | "localnet" => Ok(()),
        _ => Err(Error::Config(format!(
            "unsupported fanout network `{network}`; use mainnet, devnet, testnet, or localnet"
        ))),
    }
}

fn effective_network<'a>(configured: &'a str, override_network: Option<&'a str>) -> &'a str {
    override_network.unwrap_or(configured)
}

fn resolve_signing_account(
    network: &str,
    account_override: Option<&str>,
    keypair: Option<&Path>,
) -> Result<(Box<dyn AccountsStore>, String, Pubkey)> {
    if let Some(path) = keypair {
        let raw = std::fs::read_to_string(path).map_err(|error| {
            Error::Config(format!(
                "failed to read keypair {}: {error}",
                path.display()
            ))
        })?;
        let bytes: Vec<u8> = serde_json::from_str(&raw).map_err(|error| {
            Error::Config(format!(
                "keypair {} is not a JSON byte array: {error}",
                path.display()
            ))
        })?;
        if bytes.len() != 64 {
            return Err(Error::Config(format!(
                "keypair {} must contain exactly 64 bytes",
                path.display()
            )));
        }
        let seed: [u8; 32] = bytes[..32]
            .try_into()
            .map_err(|_| Error::Config("keypair seed must be 32 bytes".to_string()))?;
        let signing_key = SigningKey::from_bytes(&seed);
        let public = signing_key.verifying_key().to_bytes();
        if bytes[32..] != public {
            return Err(Error::Config(format!(
                "keypair {} public half does not match its secret seed",
                path.display()
            )));
        }
        let pubkey = Pubkey::new_from_array(public);
        let account_name = "fanout-keypair".to_string();
        let account = Account {
            backend: BackendKind::File,
            provider: None,
            active: true,
            auth_required: Some(false),
            pubkey: Some(pubkey.to_string()),
            vault: None,
            account: None,
            path: Some(path.to_string_lossy().to_string()),
            secret_key_b58: None,
            created_at: Some(Utc::now().to_rfc3339()),
            subscriptions: BTreeMap::new(),
        };
        let mut file = AccountsFile::default();
        file.upsert(network, &account_name, account);
        return Ok((
            Box::new(MemoryAccountsStore::with_file(file)),
            account_name,
            pubkey,
        ));
    }

    let store = FileAccountsStore::default_path();
    let file = store.load()?;
    let (account_name, account) = if let Some(name) = account_override {
        let account = file
            .named_account_for_network(network, name)
            .cloned()
            .ok_or_else(|| Error::Config(format!("account `{name}` not found on {network}")))?;
        (name.to_string(), Box::new(account))
    } else {
        match resolve_account_for_network(network, &file) {
            AccountChoice::Resolved { name, account } => (name, account),
            AccountChoice::Missing => {
                return Err(Error::Config(format!(
                    "no pay account configured for {network}; pass --account or, off mainnet, --keypair"
                )));
            }
        }
    };
    let pubkey = account
        .pubkey
        .as_deref()
        .ok_or_else(|| Error::Config(format!("account `{account_name}` has no cached pubkey")))?
        .parse()
        .map_err(|error| {
            Error::Config(format!(
                "account `{account_name}` pubkey is invalid: {error}"
            ))
        })?;
    Ok((Box::new(store), account_name, pubkey))
}

fn associated_token_address(
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Result<Pubkey> {
    let associated_program = Pubkey::from_str(programs::ASSOCIATED_TOKEN_PROGRAM)
        .map_err(|error| Error::Config(format!("invalid associated-token program id: {error}")))?;
    Ok(Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &associated_program,
    )
    .0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_types::landing::SignatureStatus;

    #[test]
    fn usdtest_is_rejected_outside_devnet() {
        let error = resolve_currency("USDtest", "mainnet").unwrap_err();
        assert!(error.to_string().contains("devnet-only"));
    }

    #[test]
    fn zero_in_flight_is_rejected_by_command_validation_shape() {
        assert_eq!(DEFAULT_MAX_IN_FLIGHT, 128);
        assert!(validate_network("devnet").is_ok());
        assert!(validate_network("bogus").is_err());
    }

    #[test]
    fn global_network_override_wins_over_fanout_default() {
        assert_eq!(effective_network("mainnet", Some("devnet")), "devnet");
        assert_eq!(effective_network("devnet", None), "devnet");
    }

    #[tokio::test]
    async fn missing_ata_rent_is_the_clusters_rent_exempt_minimum() {
        let requested = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requested.clone();
        let rpc = move |axum::Json(request): axum::Json<Value>| {
            let seen = seen.clone();
            async move {
                assert_eq!(request["method"], "getMinimumBalanceForRentExemption");
                let data_len = request["params"][0].as_u64().unwrap();
                seen.lock().unwrap().push(data_len);
                axum::Json(json!({"jsonrpc":"2.0","id":1,"result":(128 + data_len) * 5_080}))
            }
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/", axum::routing::post(rpc)),
            )
            .await
            .unwrap();
        });
        let rpc = FanoutRpc::new(format!("http://{addr}/"));
        let token: Pubkey = programs::TOKEN_PROGRAM.parse().unwrap();
        let token_2022: Pubkey = programs::TOKEN_2022_PROGRAM.parse().unwrap();

        assert_eq!(
            rpc.missing_ata_rent_lamports(&token, 3).await.unwrap(),
            3 * 1_488_440
        );
        assert_eq!(
            rpc.missing_ata_rent_lamports(&token_2022, 1).await.unwrap(),
            1_513_840
        );
        assert_eq!(rpc.missing_ata_rent_lamports(&token, 0).await.unwrap(), 0);
        assert_eq!(*requested.lock().unwrap(), vec![165, 170]);
    }
    fn manifest(rows: usize) -> TransferManifest {
        let mut csv = String::from("recipient,amount\n");
        for _ in 0..rows {
            csv.push_str(&format!("{},1\n", Pubkey::new_unique()));
        }
        parse_manifest_csv(
            csv.as_bytes(),
            ManifestContext {
                network_genesis_hash: [7; 32],
                mint: Pubkey::new_unique(),
                token_program: Pubkey::new_unique(),
                decimals: 6,
            },
        )
        .unwrap()
    }

    fn event(sequence: u64, kind: JournalEventKind) -> JournalEvent {
        JournalEvent {
            sequence,
            timestamp: Utc::now(),
            kind,
        }
    }

    fn signature(byte: u8) -> String {
        solana_signature::Signature::from([byte; 64]).to_string()
    }

    fn signed(sequence: u64, chunk_index: u32, rows: Vec<u64>, sig: u8) -> JournalEvent {
        event(
            sequence,
            JournalEventKind::ChunkSigned {
                chunk_index,
                row_numbers: rows,
                signature: signature(sig),
                signed_transaction_base64: "AA==".into(),
                blockhash: "hash".into(),
                last_valid_block_height: 100,
            },
        )
    }

    fn confirmed(sequence: u64, chunk_index: u32, sig: u8) -> JournalEvent {
        event(
            sequence,
            JournalEventKind::ChunkConfirmed {
                chunk_index,
                signature: signature(sig),
            },
        )
    }

    fn run_events(manifest: &TransferManifest, sender: &Pubkey) -> Vec<JournalEvent> {
        vec![
            event(
                1,
                JournalEventKind::RunCreated {
                    manifest_hash_hex: manifest.hash_hex(),
                    network: "devnet".into(),
                    currency: "USDC".into(),
                    mint: manifest.context.mint.to_string(),
                    token_program: manifest.context.token_program.to_string(),
                    decimals: manifest.context.decimals,
                    row_count: manifest.rows.len(),
                    total_amount_raw: manifest.total_amount_raw,
                    requested_fee_mode: "self_funded".into(),
                },
            ),
            event(
                2,
                JournalEventKind::AuthorizationGranted {
                    account_pubkey: sender.to_string(),
                    max_token_raw: manifest.total_amount_raw,
                    max_transactions: 3,
                    expires_at: Utc::now(),
                },
            ),
        ]
    }

    /// A cluster at finalized `block_height` that knows `entries`; any other
    /// signature leaves the lookup incomplete.
    struct Chain {
        entries: HashMap<String, SignatureStatus>,
        block_height: u64,
        context_slot: u64,
    }

    impl ChainState for Chain {
        type Error = Error;

        async fn finalized_tip(&self) -> Result<ChainTip> {
            Ok(ChainTip {
                slot: 50,
                block_height: self.block_height,
            })
        }

        async fn signature_statuses(&self, signatures: &[String]) -> Result<SignatureStatuses> {
            Ok(SignatureStatuses {
                context_slot: self.context_slot,
                statuses: signatures
                    .iter()
                    .filter_map(|signature| self.entries.get(signature).copied())
                    .collect(),
            })
        }
    }

    fn chain(entries: &[(u8, SignatureStatus)], block_height: u64) -> Chain {
        Chain {
            entries: entries
                .iter()
                .map(|(sig, status)| (signature(*sig), *status))
                .collect(),
            block_height,
            context_slot: 60,
        }
    }

    fn landings(chain: &Chain, events: &[JournalEvent]) -> ResumeStatuses {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(resume_landings(chain, events))
            .unwrap()
    }

    fn statuses(
        entries: &[(u8, SignatureStatus)],
        block_height: u64,
        events: &[JournalEvent],
    ) -> ResumeStatuses {
        landings(&chain(entries, block_height), events)
    }

    #[test]
    fn resume_skips_confirmed_chunks_and_replans_only_unpaid_rows() {
        let manifest = manifest(5);
        let sender = Pubkey::new_unique();
        let mut events = run_events(&manifest, &sender);
        // Chunk 0 confirmed; chunk 1 failed on chain; chunk 2 expired unseen.
        events.push(signed(3, 0, vec![2, 3], 1));
        events.push(confirmed(4, 0, 1));
        events.push(signed(5, 1, vec![4], 2));
        events.push(signed(6, 2, vec![5], 3));
        let rpc = statuses(
            &[
                (1, SignatureStatus::Confirmed),
                (2, SignatureStatus::Failed { finalized: true }),
                (3, SignatureStatus::Absent),
            ],
            101,
            &events,
        );
        verify_resume_journal(
            &events,
            &manifest,
            "devnet",
            &manifest.context.mint,
            &manifest.context.token_program,
            &sender,
        )
        .unwrap();
        let plan = plan_resume(&events, &manifest, &rpc).unwrap();
        assert_eq!(
            plan.remaining
                .rows
                .iter()
                .map(|row| row.row_number)
                .collect::<Vec<_>>(),
            vec![4, 5, 6]
        );
        assert_eq!(plan.remaining.total_amount_raw, 3_000_000);
        assert_eq!(plan.remaining.hash, manifest.hash);
        assert_eq!(plan.next_chunk_index, 3);
        assert!(plan.newly_confirmed.is_empty());
    }

    #[test]
    fn resume_records_a_confirmation_the_previous_run_missed() {
        let manifest = manifest(2);
        let sender = Pubkey::new_unique();
        let mut events = run_events(&manifest, &sender);
        events.push(signed(3, 0, vec![2, 3], 9));
        let rpc = statuses(&[(9, SignatureStatus::Confirmed)], 1, &events);
        let plan = plan_resume(&events, &manifest, &rpc).unwrap();
        assert!(plan.remaining.rows.is_empty());
        assert_eq!(plan.confirmed_chunks, 1);
        assert_eq!(plan.newly_confirmed.len(), 1);
        assert_eq!(plan.newly_confirmed[0].0, 0);
        assert_eq!(plan.newly_confirmed[0].1.to_string(), signature(9));
    }

    #[test]
    fn resume_refuses_while_a_signed_chunk_may_still_land() {
        let manifest = manifest(2);
        let sender = Pubkey::new_unique();
        let mut events = run_events(&manifest, &sender);
        events.push(signed(3, 0, vec![2], 4));
        // Unseen but its blockhash is still valid (height == last valid).
        let pending = statuses(&[(4, SignatureStatus::Absent)], 100, &events);
        assert!(
            plan_resume(&events, &manifest, &pending)
                .unwrap_err()
                .to_string()
                .contains("may still land")
        );
        // Seen but unconfirmed, or failed short of finalized, may still land.
        for status in [
            SignatureStatus::Processed,
            SignatureStatus::Failed { finalized: false },
        ] {
            let seen = statuses(&[(4, status)], 100, &events);
            assert!(plan_resume(&events, &manifest, &seen).is_err());
        }
        // A status lookup that never covered the signature is unknown.
        assert!(plan_resume(&events, &manifest, &statuses(&[], 500, &events)).is_err());
        // Statuses read from state older than the finalized tip prove nothing.
        let mut stale = chain(&[(4, SignatureStatus::Absent)], 101);
        stale.context_slot = 49;
        assert!(plan_resume(&events, &manifest, &landings(&stale, &events)).is_err());
        // One block past expiry it is provably dead and may be replaced.
        let expired = statuses(&[(4, SignatureStatus::Absent)], 101, &events);
        assert_eq!(
            plan_resume(&events, &manifest, &expired)
                .unwrap()
                .remaining
                .rows
                .len(),
            2
        );
    }

    #[test]
    fn resume_refuses_a_journal_for_another_payout_or_signer() {
        let manifest = manifest(2);
        let sender = Pubkey::new_unique();
        let events = run_events(&manifest, &sender);
        let check = |manifest: &TransferManifest, network: &str, sender: &Pubkey| {
            verify_resume_journal(
                &events,
                manifest,
                network,
                &manifest.context.mint,
                &manifest.context.token_program,
                sender,
            )
        };
        assert!(check(&manifest, "devnet", &sender).is_ok());
        assert!(check(&manifest, "mainnet", &sender).is_err());
        assert!(check(&manifest, "devnet", &Pubkey::new_unique()).is_err());
        let mut other = manifest.clone();
        other.hash = [0; 32];
        assert!(check(&other, "devnet", &sender).is_err());
        assert!(
            verify_resume_journal(
                &events[1..],
                &manifest,
                "devnet",
                &manifest.context.mint,
                &manifest.context.token_program,
                &sender,
            )
            .is_err()
        );
    }

    fn write_journal(dir: &Path, name: &str, events: &[JournalEvent]) {
        let body = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap() + "\n")
            .collect::<String>();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn fresh_run_is_refused_while_a_matching_journal_exists() {
        let manifest = manifest(2);
        let sender = Pubkey::new_unique();
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec![dir.path().to_path_buf()];
        let refuse = |sender: &Pubkey| {
            refuse_existing_runs(
                &dirs,
                &manifest.hash_hex(),
                "devnet",
                &manifest.context.mint,
                sender,
            )
        };
        // A run that never signed could not have paid anything.
        write_journal(dir.path(), "a.jsonl", &run_events(&manifest, &sender));
        assert!(refuse(&sender).is_ok());

        let mut events = run_events(&manifest, &sender);
        events.push(signed(3, 0, vec![2], 1));
        events.push(confirmed(4, 0, 1));
        write_journal(dir.path(), "a.jsonl", &events);
        let error = refuse(&sender).unwrap_err().to_string();
        assert!(error.contains("--resume"), "{error}");
        // Another account's or network's payout of the same CSV is independent.
        assert!(refuse(&Pubkey::new_unique()).is_ok());
        assert!(
            refuse_existing_runs(
                &dirs,
                &manifest.hash_hex(),
                "mainnet",
                &manifest.context.mint,
                &sender
            )
            .is_ok()
        );

        events.push(event(
            5,
            JournalEventKind::RunCompleted {
                confirmed: 1,
                failed: 0,
                unknown: 0,
            },
        ));
        write_journal(dir.path(), "a.jsonl", &events);
        let error = refuse(&sender).unwrap_err().to_string();
        assert!(error.contains("already paid"), "{error}");
    }
}
