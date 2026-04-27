//! `pay server start` — start a payment gateway proxy.

use std::process::Command as ProcessCommand;
use std::sync::Arc;

use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use owo_colors::OwoColorize;
use pay_core::PaymentState;
use pay_core::accounts::AccountsStore;
use pay_core::server::session::SessionMpp;
use pay_types::metering::{ApiSpec, OperatorConfig, SignerConfig};
use solana_mpp::server::Mpp;
use solana_mpp::solana_keychain::SolanaSigner;
use solana_mpp::solana_keychain::memory::MemorySigner;

const AUTO_OPERATOR_ACCOUNT_NAME: &str = "gateway";
const BROWSER_RPC_PROXY_PATH: &str = "/__402/rpc";
const BROWSER_RPC_ALLOWED_METHODS: &[&str] = &[
    "getLatestBlockhash",
    "surfnet_setAccount",
    "surfnet_setTokenAccount",
];

/// Start the payment gateway proxy.
///
/// Loads an API spec from a YAML file and starts an HTTP proxy that:
/// - Returns 402 with MPP challenge for metered endpoints
/// - Forwards to upstream on valid payment
/// - Passes through free endpoints directly
#[derive(clap::Args)]
pub struct StartCommand {
    /// Path to the provider YAML spec file.
    pub spec: String,

    /// Address to bind to.
    #[arg(long, default_value = "0.0.0.0:1402")]
    pub bind: String,

    /// Recipient wallet address for payments.
    #[arg(long)]
    pub recipient: Option<String>,

    /// Payment currency (SOL, USDC, etc.).
    #[arg(long, default_value = "USDC")]
    pub currency: String,

    /// RPC URL for payment verification.
    #[arg(long)]
    pub rpc_url: Option<String>,

    /// Launch the Payment Debugger UI alongside the gateway.
    /// Automatically enabled in sandbox mode (`pay --sandbox server start`).
    #[arg(long)]
    pub debugger: bool,
}

#[derive(Clone)]
struct AppState {
    apis: Arc<Vec<ApiSpec>>,
    mpps: Vec<Mpp>,
    session_mpp: Option<Arc<SessionMpp>>,
    browser_rpc_url: Option<String>,
}

impl PaymentState for AppState {
    fn apis(&self) -> &[ApiSpec] {
        &self.apis
    }
    fn mpp(&self) -> Option<&Mpp> {
        self.mpps.first()
    }
    fn mpps(&self) -> Vec<&Mpp> {
        self.mpps.iter().collect()
    }
    fn browser_rpc_url(&self) -> Option<&str> {
        self.browser_rpc_url.as_deref()
    }
    fn session_mpp(&self) -> Option<&SessionMpp> {
        self.session_mpp.as_deref()
    }
}

impl StartCommand {
    pub fn run(self, active_account_name: Option<&str>, sandbox: bool) -> pay_core::Result<()> {
        let debugger = self.debugger || sandbox;
        let expanded = shellexpand::tilde(&self.spec);
        let contents = std::fs::read_to_string(expanded.as_ref())
            .map_err(|e| pay_core::Error::Config(format!("Failed to read {}: {e}", self.spec)))?;

        let api: ApiSpec = serde_yml::from_str(&contents)
            .map_err(|e| pay_core::Error::Config(format!("Invalid spec: {e}")))?;

        // Apply env vars from spec (static values or ${VAR} passthrough).
        // SAFETY: called before any threads are spawned.
        for (key, value) in &api.env {
            if value.starts_with("${") && value.ends_with('}') {
                let var_name = &value[2..value.len() - 1];
                if let Ok(v) = std::env::var(var_name) {
                    unsafe { std::env::set_var(key, v) };
                }
            } else {
                unsafe { std::env::set_var(key, value) };
            }
        }

        let op = api.operator.clone();
        let op = op.as_ref();

        // Note: we used to refuse any `operator.signer` block unless the
        // `gcp_kms` feature was built. That was over-broad — the new
        // `Account` and `File` variants need no extra build features and
        // are the recommended path for local dev. The per-variant gate
        // now lives inside `resolve_signer` itself.

        // Resolve config that doesn't need async.
        let currencies = resolve_operator_currencies(op, &self.currency);

        let network = op
            .and_then(|o| o.network.clone())
            .unwrap_or_else(|| "mainnet".to_string());

        // RPC URL fallback chain. Network-aware so that `localnet`
        // defaults to the hosted Surfpool sandbox (where ephemeral
        // wallets can be auto-created and auto-funded). Users running
        // a real `solana-test-validator` should set `operator.rpc_url`
        // explicitly or pass `--rpc-url`.
        let rpc_url = op
            .and_then(|o| o.rpc_url.clone())
            .or(self.rpc_url.clone())
            .or_else(|| std::env::var("PAY_RPC_URL").ok())
            .unwrap_or_else(|| match network.as_str() {
                // localnet → Surfpool sandbox (lets the smart-default
                // auto-create + auto-fund the ephemeral on first run).
                "localnet" => pay_core::config::SANDBOX_RPC_URL.to_string(),
                "devnet" => "https://api.devnet.solana.com".to_string(),
                "mainnet" => "https://api.mainnet-beta.solana.com".to_string(),
                // Unknown network → fall through to the old default.
                _ => {
                    if sandbox {
                        pay_core::config::SANDBOX_RPC_URL.to_string()
                    } else {
                        pay_core::config::LOCAL_RPC_URL.to_string()
                    }
                }
            });

        let fee_payer = op.map(|o| o.fee_payer).unwrap_or(false);
        let signer_cfg = op.and_then(|o| o.signer.clone());
        let active_account_name_owned = active_account_name.map(|s| s.to_string());

        // Create the runtime first — everything async runs inside it so
        // background tasks (like GCP auth token refresh) stay alive.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| pay_core::Error::Config(format!("Failed to create runtime: {e}")))?;

        rt.block_on(async {
            // ── Resolve fee-payer signer (async — needs the runtime) ──
            //
            // Lookup order, first match wins:
            //
            //   1. **Explicit `operator.signer` in YAML** — user said
            //      what they want, honor it. Handles GcpKms (build-
            //      feature gated), Account (named entry in
            //      accounts.yml), and File (JSON keypair on disk).
            //
            //   2. **`--sandbox` flag** — forces a dedicated localnet
            //      gateway ephemeral from accounts.yml regardless of
            //      network. Lazy-creates on first use. Never touches the
            //      keychain.
            //
            //   3. **Throwaway network slug** (`localnet` / `devnet`) —
            //      smart default: route through the network-aware
            //      loader so users running `pay server start` against
            //      a localnet/devnet spec don't have to think about
            //      signers. Same code path as the sandbox flag.
            //
            //   4. **None** — leaves fee_payer_signer empty. Caught by
            //      the early-validation guard below if `fee_payer: true`.
            let fee_payer_signer: Option<Arc<dyn SolanaSigner>> = if let Some(ref cfg) = signer_cfg
            {
                Some(resolve_signer(cfg).await?)
            } else if sandbox || matches!(network.as_str(), "localnet" | "devnet") {
                let auto_network = if sandbox { "localnet" } else { network.as_str() };
                let store = pay_core::accounts::FileAccountsStore::default_path();
                let _ = pay_core::accounts::load_or_create_exact_ephemeral_for_network_as(
                    auto_network,
                    pay_core::accounts::DEFAULT_ACCOUNT_NAME,
                    &store,
                )?;
                let (signer, ephemeral_notice) =
                    pay_core::signer::load_signer_for_network_with_reason(
                        auto_network,
                        &store,
                        Some(AUTO_OPERATOR_ACCOUNT_NAME),
                        "use your pay account as the gateway fee payer",
                    )?;
                if let Some(resolved) = ephemeral_notice {
                    eprintln!(
                        "  {} {} {}",
                        "Generated".green(),
                        resolved.network.bold(),
                        format!(
                            "{} wallet ({})",
                            resolved.account_name,
                            resolved.account.pubkey.as_deref().unwrap_or("?")
                        )
                        .dimmed()
                    );
                }
                Some(Arc::new(signer) as Arc<dyn SolanaSigner>)
            } else if let Some(ref source) = active_account_name_owned {
                // Mainnet (or unknown network) with no `operator.signer`
                // block but a default keypair from `pay setup` —
                // typically `keychain:default`. Load it once at startup
                // with a meaningful reason string so the OS auth prompt
                // tells the user *why* it's being asked. The same
                // signer is then used as both the fee-payer and the
                // recipient-pubkey source (no second load).
                let intent = pay_core::keystore::AuthIntent::use_gateway_fee_payer();
                let signer = pay_core::signer::load_signer_with_intent(source, &intent)?;
                Some(Arc::new(signer) as Arc<dyn SolanaSigner>)
            } else {
                None
            };

            // ── Resolve recipient ──
            //
            // Lookup order (first match wins):
            //   1. operator.recipient in YAML
            //   2. --recipient flag
            //   3. PAY_PAYMENT_RECIPIENT env var
            //   4. fee_payer_signer's pubkey — covers sandbox, throwaway-
            //      network smart default, explicit operator.signer block,
            //      and the active_account_name fallback (all four set
            //      fee_payer_signer above).
            let recipient = if let Some(r) = op.and_then(|o| o.recipient.as_ref()) {
                r.clone()
            } else if let Some(r) = &self.recipient {
                r.clone()
            } else if let Ok(r) = std::env::var("PAY_PAYMENT_RECIPIENT") {
                r
            } else if let Some(ref signer) = fee_payer_signer {
                signer.pubkey().to_string()
            } else {
                return Err(pay_core::Error::Config(
                    "No recipient specified. Use operator.recipient in YAML, --recipient flag, PAY_PAYMENT_RECIPIENT env, or `pay setup`."
                        .to_string(),
                ));
            };

            // ── Validate fee_payer / signer consistency ──
            //
            // If the operator YAML demands `fee_payer: true` (the
            // server co-signs to sponsor transaction fees) but no
            // signer is available, the server would start happily and
            // then fail every single payment at verify time with the
            // unhelpful "Fee payer enabled but no signer configured"
            // error. Catch it at startup instead so the user knows
            // immediately what to fix.
            if fee_payer && fee_payer_signer.is_none() {
                return Err(pay_core::Error::Config(
                    "operator.fee_payer is `true` but no fee payer signer is configured.\n\n\
                     In sandbox mode, start the server with `pay --sandbox server start ...` \
                     (or use `pay -s server demo`).\n\
                     In production, set `operator.signer` in the YAML (requires the \
                     `gcp_kms` build feature) or set `operator.fee_payer: false` \
                     so clients pay their own fees."
                        .to_string(),
                ));
            }

            // ── Auto-fund the operator wallet on Surfpool ──
            //
            // Trigger when EITHER:
            //   - the user passed `--sandbox` (explicit opt-in), or
            //   - the resolved RPC URL points at Surfpool (the smart-
            //     default `network: localnet` path lands here).
            //
            // `fund_via_surfpool` deposits a fixed amount (100 SOL +
            // 1000 USDC) so calling it on every server start is
            // idempotent and survives Surfpool restarts (which would
            // otherwise wipe the cheatcode-set balances).
            //
            // When the RPC is a real cluster (mainnet/devnet/local
            // validator), funding is skipped silently — the operator
            // is responsible for funding their own wallet.
            let looks_like_surfpool =
                rpc_url.contains("surfnet") || rpc_url.contains("surfpool");
            let should_fund = sandbox || looks_like_surfpool;
            if should_fund && let Some(ref signer) = fee_payer_signer {
                let pubkey = signer.pubkey().to_string();
                if let Err(e) =
                    pay_core::client::sandbox::fund_via_surfpool(&rpc_url, &pubkey).await
                {
                    eprintln!(
                        "  {} {}",
                        "Sandbox funding failed:".red(),
                        e.to_string().dimmed()
                    );
                }
            }

            // ── Create MPP servers ──
            let secret_key = std::env::var("PAY_MPP_CHALLENGE_SECRET")
                .unwrap_or_else(|_| bs58::encode(rand::random::<[u8; 32]>()).into_string());

            let currency_configs: Vec<_> = currencies
                .iter()
                .map(|currency| {
                    let (mpp_currency, decimals) = resolve_currency(currency, &network);
                    (currency.clone(), mpp_currency, decimals)
                })
                .collect();
            let mpps: Vec<Mpp> = currency_configs
                .iter()
                .map(|(_, mpp_currency, decimals)| {
                    Mpp::new(solana_mpp::server::Config {
                        recipient: recipient.clone(),
                        currency: mpp_currency.clone(),
                        decimals: *decimals,
                        network: network.clone(),
                        rpc_url: Some(rpc_url.clone()),
                        secret_key: Some(secret_key.clone()),
                        fee_payer,
                        fee_payer_signer: fee_payer_signer.clone(),
                        html: true,
                        ..Default::default()
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| pay_core::Error::Config(format!("Failed to create MPP server: {e}")))?;
            let (_session_currency, session_mpp_currency, session_decimals) =
                currency_configs.first().cloned().ok_or_else(|| {
                    pay_core::Error::Config(
                        "At least one operator currency must be configured".to_string(),
                    )
                })?;

            // ── Create session MPP server (if session config present) ──
            let session_mpp: Option<Arc<SessionMpp>> = if let Some(ref sess) = api.session {
                use pay_core::server::session::RpcMultiDelegateChain;
                use solana_mpp::program::multi_delegator::MULTI_DELEGATOR_PROGRAM_ID;
                use solana_mpp::server::session::SessionConfig;
                use solana_mpp::SessionMode;
                use std::str::FromStr;

                let cap_base = (sess.cap_usdc * 10f64.powi(session_decimals as i32)).round() as u64;
                let session_secret = std::env::var("PAY_SESSION_SECRET")
                    .unwrap_or_else(|_| secret_key.clone());
                let modes: Vec<SessionMode> = if sess.modes.is_empty() {
                    vec![SessionMode::Push]
                } else {
                    sess.modes
                        .iter()
                        .map(|m| match m.as_str() {
                            "pull" => SessionMode::Pull,
                            _ => SessionMode::Push,
                        })
                        .collect()
                };
                let using_local_rpc = rpc_url.contains("localhost") || rpc_url.contains("127.0.0.1");
                let fiber_program_id = if using_local_rpc {
                    Some(ensure_local_fiber_program(&rpc_url)?)
                } else {
                    std::env::var("PAY_FIBER_PROGRAM_ID")
                        .ok()
                        .and_then(|value| solana_pubkey::Pubkey::from_str(&value).ok())
                };

                let config = SessionConfig {
                    recipient: recipient.clone(),
                    operator: recipient.clone(),
                    currency: session_mpp_currency.clone(),
                    decimals: session_decimals,
                    network: network.clone(),
                    max_cap: cap_base,
                    min_voucher_delta: sess.min_voucher_delta,
                    modes: modes.clone(),
                    rpc_url: Some(rpc_url.clone()),
                    program_id: fiber_program_id,
                    ..Default::default()
                };

                let mut smpp = SessionMpp::new(config, session_secret)
                    .with_realm(api.title.clone());

                // Wire up the multi-delegate chain when pull mode is enabled.
                if modes.contains(&SessionMode::Pull) {
                    let program_id = solana_pubkey::Pubkey::from_str(MULTI_DELEGATOR_PROGRAM_ID)
                        .expect("valid multi-delegator program ID");
                    let mint = solana_pubkey::Pubkey::from_str(&session_mpp_currency)
                        .unwrap_or_else(|_| {
                            // fallback: mainnet USDC
                            solana_pubkey::Pubkey::from_str(
                                "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                            )
                            .unwrap()
                        });
                    let operator_pk = solana_pubkey::Pubkey::from_str(&recipient)
                        .unwrap_or_else(|_| solana_pubkey::Pubkey::default());

                    if using_local_rpc {
                        ensure_local_multi_delegator_program(&rpc_url, MULTI_DELEGATOR_PROGRAM_ID)?;
                    }

                    if sandbox && !using_local_rpc {
                        // Sandbox: record submitted txs and return a stub sig so
                        // the full HTTP flow works without a live multi-delegator
                        // program on Surfpool.
                        use pay_core::server::session::MultiDelegateChain;
                        use solana_mpp::program::multi_delegator::MultiDelegateOnChainState;
                        use std::future::Future;
                        use std::pin::Pin;

                        struct SandboxChain;

                        impl MultiDelegateChain for SandboxChain {
                            fn fetch_state<'a>(
                                &'a self,
                                owner: &'a str,
                            ) -> Pin<
                                Box<
                                    dyn Future<
                                            Output = pay_core::Result<
                                                MultiDelegateOnChainState,
                                            >,
                                        > + Send
                                        + 'a,
                                >,
                            > {
                                let _ = owner;
                                Box::pin(async {
                                    Ok(MultiDelegateOnChainState {
                                        multi_delegate_exists: false,
                                        existing_delegation_cap: None,
                                    })
                                })
                            }

                            fn submit_tx<'a>(
                                &'a self,
                                tx_base64: &'a str,
                            ) -> Pin<
                                Box<
                                    dyn Future<Output = pay_core::Result<String>>
                                        + Send
                                        + 'a,
                                >,
                            > {
                                let preview = &tx_base64[..40.min(tx_base64.len())];
                                eprintln!("  {} {preview}…", "[sandbox] submit_tx".dimmed());
                                Box::pin(async { Ok("sandbox_stub_sig".to_string()) })
                            }
                        }

                        smpp = smpp.with_multi_delegate_chain(Box::new(SandboxChain));
                    } else {
                        smpp = smpp.with_multi_delegate_chain(Box::new(RpcMultiDelegateChain {
                            rpc_url: rpc_url.clone(),
                            program_id,
                            mint,
                            operator: operator_pk,
                            delegation_nonce: 0,
                        }));
                    }

                    let operator_signer = fee_payer_signer.clone().ok_or_else(|| {
                        pay_core::Error::Config(
                            "pull-mode sessions require an operator signer".to_string(),
                        )
                    })?;
                    let fiber_program_id = fiber_program_id.ok_or_else(|| {
                        pay_core::Error::Config(
                            "pull-mode sessions require a Fiber program ID; set PAY_FIBER_PROGRAM_ID or use local RPC"
                                .to_string(),
                        )
                    })?;

                    smpp = smpp.with_open_channel_batcher(
                        operator_signer,
                        rpc_url.clone(),
                        fiber_program_id,
                        sess.batch_open_interval_ms,
                    );
                    tracing::info!(
                        interval_ms = sess.batch_open_interval_ms,
                        fiber_program_id = %fiber_program_id,
                        "enabled pull-mode Fiber batcher"
                    );
                }

                Some(Arc::new(smpp))
            } else {
                None
            };

            // ── Banner ──
            let metered_count = api
                .endpoints
                .iter()
                .filter(|e| e.metering.is_some())
                .count();
            let free_count = api.endpoints.len() - metered_count;

            eprintln!();
            eprintln!("  {}   {}", "╔═╗ ╔═╗ ╦ ╦".bold(), "╔═╗ ╦ ╦".dimmed());
            eprintln!("  {}   {}", "╠═╝ ╠═╣ ╚╦╝".bold(), "╚═╗ ╠═╣".dimmed());
            eprintln!("  {}  {} {}", "╩   ╩ ╩  ╩".bold(), "○".dimmed(), "╚═╝ ╩ ╩".dimmed());
            eprintln!("  {}", "Developer Tools for Programmable Payments".dimmed());
            eprintln!();

            // Network link
            let network_label = if sandbox { "sandbox" } else { &network };
            let network_url = if sandbox {
                if rpc_url.contains("localhost") || rpc_url.contains("127.0.0.1") {
                    "http://localhost:18488".to_string()
                } else {
                    rpc_url.clone()
                }
            } else {
                "https://explorer.solana.com".to_string()
            };
            let network_link = crate::components::link::link_with_arrow(network_label, &network_url);

            // Operator link (explorer token page). The cluster query
            // segment depends on the network:
            //   - mainnet → no param (mainnet is the explorer default)
            //   - devnet  → ?cluster=devnet
            //   - localnet/sandbox → ?cluster=custom&customUrl=<rpc>
            let short_recipient = if recipient.len() > 8 {
                format!("{}...{}", &recipient[..4], &recipient[recipient.len() - 4..])
            } else {
                recipient.clone()
            };
            let cluster_query = explorer_cluster_query(&network, &rpc_url);
            let operator_url = format!(
                "https://explorer.solana.com/address/{}/tokens{}",
                recipient, cluster_query
            );
            let operator_link = crate::components::link::link_with_arrow(&short_recipient, &operator_url);

            eprintln!("  {}\t{}", "network".dimmed(), network_link);
            eprintln!(
                "  {}\t{} via {}",
                "currency".dimmed(),
                "$".green(),
                currencies.join(", ").green()
            );

            // Fetch the operator wallet's SOL balance for the banner.
            // We do this in BOTH sandbox and mainnet modes — in sandbox
            // it confirms the auto-fund worked; on mainnet it's a quick
            // sanity check that the wallet actually exists on chain so
            // the user doesn't waste time hitting the gateway with a
            // wallet that has zero SOL.
            //
            // Color thresholds (covers all networks):
            //   ≥ 0.10 SOL  → green   (comfortable runway)
            //   ≥ 0.05 SOL  → yellow  (top up soon)
            //    < 0.05 SOL → red     (next tx may fail)
            let operator_sol = fetch_sol_balance(&rpc_url, &recipient).await;
            let balance_text = format!(" ({} SOL)", format_price(operator_sol));
            let balance_colored = if operator_sol >= 0.10 {
                balance_text.green().to_string()
            } else if operator_sol >= 0.05 {
                balance_text.yellow().to_string()
            } else {
                balance_text.red().to_string()
            };
            eprintln!("  {}\t{}{}", "operator".dimmed(), operator_link, balance_colored);
            eprintln!();

            // Loud warning when the wallet is empty. The most common
            // first-run failure mode on mainnet is "Attempt to debit an
            // account but found no record of a prior credit" — a Solana
            // runtime error that means the wallet has zero SOL on the
            // configured cluster. Tell the user upfront instead of
            // letting every payment fail at simulation time.
            if operator_sol == 0.0 {
                eprintln!(
                    "{}",
                    crate::components::notice(
                        crate::components::NoticeLevel::Warning,
                        "Operator wallet has 0 SOL",
                        &format!(
                            "{recipient}\non `{network}` via {rpc_url}\n\n\
                             Even with operator.fee_payer = true, the wallet must \
                             exist on chain (any prior credit) for SPL token \
                             transfers to derive an ATA. Send a small amount of \
                             SOL to the address above and restart the server."
                        ),
                    )
                );
            }

            eprintln!(
                "  {}",
                format!(
                    "{} endpoints ({} metered, {} free)",
                    api.endpoints.len(),
                    metered_count,
                    free_count
                )
                .dimmed()
            );
            eprintln!();

            let max_path_len = api
                .endpoints
                .iter()
                .map(|e| e.path.len())
                .max()
                .unwrap_or(20);

            let rule = format!(
                "  {}{}{}", "─".repeat(9), "─".repeat(max_path_len + 2), "─".repeat(10)
            );
            eprintln!("{}", rule.dimmed());

            for ep in &api.endpoints {
                let method = format!("{:?}", ep.method).to_uppercase();
                let method_padded = format!("{:<7}", method);
                let method_colored = match method.as_str() {
                    "GET" => method_padded.green().to_string(),
                    "POST" => method_padded.blue().to_string(),
                    "PUT" => method_padded.yellow().to_string(),
                    "DELETE" => method_padded.red().to_string(),
                    "PATCH" => method_padded.cyan().to_string(),
                    _ => method_padded.dimmed().to_string(),
                };
                let price_tag = if let Some(ref m) = ep.metering {
                    let price = m
                        .dimensions
                        .first()
                        .map(|d| d.tiers.first().map(|t| t.price_usd).unwrap_or(0.0))
                        .or_else(|| {
                            m.variants
                                .first()
                                .and_then(|v| v.dimensions.first())
                                .and_then(|d| d.tiers.first())
                                .map(|t| t.price_usd)
                        })
                        .unwrap_or(0.0);
                    format!("{:>8}", format!("${}", format_price(price)))
                        .yellow()
                        .to_string()
                } else {
                    format!("{:>8}", "free").green().to_string()
                };

                let path_url = format!("http://{}/{}", self.bind.replace("0.0.0.0", "127.0.0.1"), ep.path.trim_start_matches('/'));
                let path_linked = crate::components::link::link_with_arrow(&ep.path, &path_url);
                // Pad after the link (padding itself is not clickable)
                let padding = " ".repeat(max_path_len.saturating_sub(ep.path.len()));
                eprintln!(
                    "  {} {}{} {}",
                    method_colored,
                    path_linked,
                    padding,
                    price_tag,
                );
            }

            eprintln!("{}", rule.dimmed());

            eprintln!();

            // ── Build router ──
            let endpoints_json = build_endpoints_json(&api);

            let verify_mpps = mpps.clone();

            let state = AppState {
                apis: Arc::new(vec![api.clone()]),
                mpps,
                session_mpp,
                browser_rpc_url: Some(BROWSER_RPC_PROXY_PATH.to_string()),
            };

            let pdb_state = if debugger {
                let pdb_config = build_pdb_config(&api, &recipient, &network, &rpc_url);
                let pdb = pay_pdb::PdbState::new(pdb_config);
                pdb.spawn_cleanup();
                Some(pdb)
            } else {
                None
            };

            let verify_pdb = pdb_state.clone();
            let rpc_proxy_url = rpc_url.clone();
            let rpc_proxy_client = reqwest::Client::new();
            let mut app = axum::Router::new()
                .route("/__402/health", get(|| async { "ok" }))
                .route(
                    BROWSER_RPC_PROXY_PATH,
                    post(move |body: axum::body::Bytes| {
                        let client = rpc_proxy_client.clone();
                        let rpc_url = rpc_proxy_url.clone();
                        async move { browser_rpc_proxy(client, rpc_url, body).await }
                    }),
                )
                .route(
                    "/__402/endpoints",
                    get(move || async move { axum::Json(endpoints_json).into_response() }),
                )
                .route(
                    "/__402/verify",
                    post(move |body: axum::Json<GatewayVerifyRequest>| async move {
                        gateway_verify(verify_mpps.clone(), body.0, verify_pdb.as_ref()).await
                    }),
                );

            if let Some(ref pdb) = pdb_state {
                app = app
                    .route(
                        "/",
                        get(|headers: axum::http::HeaderMap| async move {
                            let accepts_html = headers
                                .get("accept")
                                .and_then(|v| v.to_str().ok())
                                .is_some_and(|v| v.contains("text/html"));
                            if accepts_html {
                                axum::response::Redirect::temporary(
                                        &format!("{}/", pay_pdb::PDB_PATH),
                                    )
                                    .into_response()
                            } else {
                                axum::Json(serde_json::json!({"status": "ok"})).into_response()
                            }
                        }),
                    )
                    .nest_service(
                        pay_pdb::PDB_PATH,
                    pay_pdb::debugger_router(pdb.clone()),
                );
            }

            let app = app
                .fallback(any(move |req: axum::http::Request<axum::body::Body>| {
                    let api = api.clone();
                    async move {
                        let (parts, body) = req.into_parts();
                        // 404 for paths not listed in the spec — prevents OAuth2
                        // token fetches for browser auto-requests like /favicon.ico.
                        let path = parts.uri.path().trim_start_matches('/');
                        if pay_core::server::metering::find_endpoint_by_path(&api, path).is_none() {
                            return axum::response::IntoResponse::into_response((
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({"error": "not_found"})),
                            ));
                        }
                        let bytes = axum::body::to_bytes(body, 10 * 1024 * 1024)
                            .await
                            .unwrap_or_default();
                        pay_core::server::proxy::forward_request(
                            &api,
                            parts.method,
                            &parts.uri,
                            &parts.headers,
                            bytes,
                        )
                        .await
                        .unwrap_or_else(|e| e)
                    }
                }))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    pay_core::server::payment::payment_middleware::<AppState>,
                ))
                .with_state(state)
                // Logging layer (outermost — executes first).
                // Extension must be added AFTER the middleware layer (LIFO order)
                // so the extension is available when the middleware runs.
                .layer(middleware::from_fn(pay_pdb::logging::logging_middleware))
                .layer(axum::Extension(pdb_state));

            let listener = tokio::net::TcpListener::bind(&self.bind)
                .await
                .map_err(|e| {
                    pay_core::Error::Config(format!("Failed to bind {}: {e}", self.bind))
                })?;
            let display_addr = self.bind.replace("0.0.0.0", "127.0.0.1");
            let url = format!("http://{}", display_addr);
            if debugger {
                eprintln!(
                    "  {} {}",
                    "Running Payment debugger".green().bold(),
                    crate::components::link::link_with_arrow(&url, &url),
                );
            } else {
                eprintln!(
                    "  {} {}",
                    "listening".green().bold(),
                    crate::components::link::link_with_arrow(&url, &url),
                );
            }
            eprintln!();
            axum::serve(listener, app)
                .await
                .map_err(|e| pay_core::Error::Config(format!("Server error: {e}")))
        })
    }
}

fn build_endpoints_json(api: &ApiSpec) -> serde_json::Value {
    let endpoints: Vec<serde_json::Value> = api
        .endpoints
        .iter()
        .map(|ep| {
            let mut obj = serde_json::json!({
                "method": format!("{:?}", ep.method).to_uppercase(),
                "path": ep.path,
                "metered": ep.metering.is_some(),
            });
            if let Some(desc) = &ep.description {
                obj["description"] = serde_json::Value::String(desc.clone());
            }
            if let Some(ref m) = ep.metering {
                let price = m
                    .dimensions
                    .first()
                    .map(|d| d.tiers.first().map(|t| t.price_usd).unwrap_or(0.0))
                    .unwrap_or(0.0);
                obj["price_usd"] = serde_json::json!(price);
            }
            obj
        })
        .collect();

    serde_json::json!({
        "name": api.name,
        "title": api.title,
        "forward": {
            "url": api.routing.display_url(),
        },
        "endpoints": endpoints,
    })
}

/// Build the sidebar config for the PDB frontend.
fn build_pdb_config(
    api: &ApiSpec,
    recipient: &str,
    network: &str,
    rpc_url: &str,
) -> serde_json::Value {
    let metered: Vec<serde_json::Value> = api
        .endpoints
        .iter()
        .filter(|e| e.metering.is_some())
        .map(|e| {
            let price = e
                .metering
                .as_ref()
                .and_then(|m| m.dimensions.first())
                .and_then(|d| d.tiers.first())
                .map(|t| format!("${}", format_price(t.price_usd)))
                .unwrap_or_else(|| "metered".into());
            serde_json::json!({
                "method": format!("{:?}", e.method).to_uppercase(),
                "path": e.path,
                "price": price,
                "description": e.description.as_deref().unwrap_or(""),
            })
        })
        .collect();

    let free: Vec<serde_json::Value> = api
        .endpoints
        .iter()
        .filter(|e| e.metering.is_none())
        .map(|e| {
            serde_json::json!({
                "method": format!("{:?}", e.method).to_uppercase(),
                "path": e.path,
                "price": "free",
                "description": e.description.as_deref().unwrap_or(""),
            })
        })
        .collect();

    serde_json::json!({
        "recipient": recipient,
        "network": network,
        "rpcUrl": rpc_url,
        "endpoints": {
            "mpp": metered,
            "x402": [],
            "oauth": free,
        }
    })
}

async fn browser_rpc_proxy(
    client: reqwest::Client,
    rpc_url: String,
    body: axum::body::Bytes,
) -> Response {
    if let Err(message) = validate_browser_rpc_request(&body) {
        return rpc_proxy_error(axum::http::StatusCode::BAD_REQUEST, message);
    }

    let upstream = match client
        .post(&rpc_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(error = %error, "Browser RPC proxy request failed");
            return rpc_proxy_error(
                axum::http::StatusCode::BAD_GATEWAY,
                "Payment RPC is unavailable.",
            );
        }
    };

    let status = axum::http::StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = match upstream.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(error = %error, "Browser RPC proxy response read failed");
            return rpc_proxy_error(
                axum::http::StatusCode::BAD_GATEWAY,
                "Payment RPC response could not be read.",
            );
        }
    };

    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::from(bytes))
        .unwrap()
}

fn validate_browser_rpc_request(body: &[u8]) -> Result<(), &'static str> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "Payment RPC request must be valid JSON.")?;

    let calls: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Object(_) => vec![&value],
        serde_json::Value::Array(calls) if !calls.is_empty() => calls.iter().collect(),
        _ => return Err("Payment RPC request must be a JSON-RPC object."),
    };

    for call in calls {
        let method = call
            .get("method")
            .and_then(|method| method.as_str())
            .ok_or("Payment RPC request is missing a method.")?;
        if !BROWSER_RPC_ALLOWED_METHODS.contains(&method) {
            return Err("Payment RPC method is not allowed.");
        }
    }

    Ok(())
}

fn rpc_proxy_error(status: axum::http::StatusCode, message: &'static str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "error": "payment_rpc_failed",
            "message": message,
        })),
    )
        .into_response()
}

/// Resolve a currency label to the value used in the MPP challenge.
/// SPL tokens use their mint address; SOL uses "sol".
fn resolve_currency(currency: &str, network: &str) -> (String, u8) {
    match currency.to_uppercase().as_str() {
        "SOL" => ("sol".to_string(), 9),
        "USDC" | "USDT" | "PYUSD" | "CASH" => (
            solana_mpp::protocol::solana::resolve_stablecoin_mint(currency, Some(network))
                .unwrap_or(currency)
                .to_string(),
            6,
        ),
        other => (other.to_string(), 6),
    }
}

fn resolve_operator_currencies(op: Option<&OperatorConfig>, cli_currency: &str) -> Vec<String> {
    let configured = op
        .and_then(|operator| operator.currencies.get("usd"))
        .filter(|currencies| !currencies.is_empty())
        .cloned()
        .unwrap_or_else(|| vec![cli_currency.to_string()]);

    let mut deduped = Vec::new();
    for currency in configured {
        let currency = currency.trim();
        if currency.is_empty() {
            continue;
        }
        if !deduped
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(currency))
        {
            deduped.push(currency.to_string());
        }
    }

    if deduped.is_empty() {
        vec![cli_currency.to_string()]
    } else {
        deduped
    }
}

/// Create a SolanaSigner from the operator.signer config.
///
/// Production wrapper around [`resolve_signer_with_store`] that uses the
/// real on-disk accounts file. Tests use the lower-level function with a
/// `MemoryAccountsStore` so they don't touch `~/.config/pay/accounts.yml`.
///
/// Must be called from within the main async runtime so the GCP auth
/// token cache's background refresh tasks stay alive.
async fn resolve_signer(config: &SignerConfig) -> pay_core::Result<Arc<dyn SolanaSigner>> {
    let store = pay_core::accounts::FileAccountsStore::default_path();
    resolve_signer_with_store(config, &store).await
}

/// Testable core: same as [`resolve_signer`] but takes the accounts
/// store as a parameter.
///
/// Handles all `SignerConfig` variants. The GCP KMS branch is feature-
/// gated because it pulls in the gcp-auth crate; the Account and File
/// branches need no extra build features.
async fn resolve_signer_with_store(
    config: &SignerConfig,
    store: &dyn AccountsStore,
) -> pay_core::Result<Arc<dyn SolanaSigner>> {
    match config {
        #[cfg(feature = "gcp_kms")]
        SignerConfig::GcpKms { key_name, pubkey } => {
            let signer =
                solana_mpp::solana_keychain::GcpKmsSigner::new(key_name.clone(), pubkey.clone())
                    .await
                    .map_err(|e| {
                        pay_core::Error::Config(format!("Failed to create GCP KMS signer: {e}"))
                    })?;
            Ok(Arc::new(signer))
        }
        #[cfg(not(feature = "gcp_kms"))]
        SignerConfig::GcpKms { .. } => Err(pay_core::Error::Config(
            "operator.signer.backend = gcp-kms requires the `gcp_kms` build feature. \
             Rebuild pay with `cargo build --features gcp_kms`, or use \
             `backend: account` / `backend: file` instead."
                .to_string(),
        )),
        SignerConfig::Account { name } => {
            // Resolve through the accounts file. For keychain-backed
            // accounts this triggers the OS auth prompt ONCE here (at
            // server startup), then the loaded signer is reused for
            // every payment — no per-request prompt.
            //
            // Search mainnet first, then any other network, for the
            // named account.
            let file = store.load()?;
            let (network, account) = file
                .accounts
                .get(pay_core::accounts::MAINNET_NETWORK)
                .and_then(|net| {
                    net.get(name)
                        .map(|account| (pay_core::accounts::MAINNET_NETWORK, account))
                })
                .or_else(|| {
                    file.accounts.iter().find_map(|(network, net)| {
                        net.get(name).map(|account| (network.as_str(), account))
                    })
                })
                .ok_or_else(|| {
                    pay_core::Error::Config(format!(
                        "operator.signer.name = `{name}` does not exist in \
                         ~/.config/pay/accounts.yml. Run `pay account ls` to see \
                         available accounts, or `pay setup` to create one."
                    ))
                })?;
            // Use the Account's load path so ephemeral entries work too.
            let signer = if account.keystore == pay_core::accounts::Keystore::Ephemeral {
                let bytes = account.ephemeral_keypair_bytes().ok_or_else(|| {
                    pay_core::Error::Config(format!(
                        "Account `{name}` is ephemeral but has no inline secret_key_b58"
                    ))
                })?;
                solana_mpp::solana_keychain::MemorySigner::from_bytes(&bytes).map_err(|e| {
                    pay_core::Error::Config(format!("Invalid keypair bytes for `{name}`: {e}"))
                })?
            } else {
                let intent = pay_core::keystore::AuthIntent::use_gateway_fee_payer();
                pay_core::signer::load_signer_from_account_with_intent(
                    account, name, network, &intent,
                )?
            };
            Ok(Arc::new(signer))
        }
        SignerConfig::File { path } => {
            let expanded = shellexpand::tilde(path).into_owned();
            let intent = pay_core::keystore::AuthIntent::use_gateway_fee_payer();
            let signer =
                pay_core::signer::load_signer_with_intent(&expanded, &intent).map_err(|e| {
                    pay_core::Error::Config(format!(
                        "operator.signer.path = `{path}` could not be loaded: {e}.\n\n\
                     Expected a Solana CLI keypair file (a JSON array of exactly \
                     64 bytes: 32 bytes secret + 32 bytes public key).\n\n\
                     Generate one with `solana-keygen new -o {path}`."
                    ))
                })?;
            Ok(Arc::new(signer))
        }
    }
}

/// Build the `?cluster=...` query suffix for a Solana Explorer URL.
///
/// - `mainnet`            → empty string (mainnet is the explorer default)
/// - `devnet`             → `?cluster=devnet`
/// - `localnet` (any RPC) → `?cluster=custom&customUrl=<rpc>` so the
///   explorer talks to the local validator (Surfpool sandbox or real
///   localhost) instead of guessing.
/// - anything else        → empty string (let the explorer decide)
fn explorer_cluster_query(network: &str, rpc_url: &str) -> String {
    match network {
        "mainnet" => String::new(),
        "devnet" => "?cluster=devnet".to_string(),
        "localnet" => format!("?cluster=custom&customUrl={}", urlencoding::encode(rpc_url)),
        _ => String::new(),
    }
}

/// Fetch a wallet's lamport balance via JSON-RPC. Returns 0 on any error
/// — used by the banner only, where a missing balance is harmless.
async fn fetch_lamports(client: &reqwest::Client, rpc_url: &str, pubkey: &str) -> u64 {
    let resp = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBalance",
            "params": [pubkey]
        }))
        .send()
        .await;
    match resp {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["result"]["value"].as_u64())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

async fn fetch_sol_balance(rpc_url: &str, pubkey: &str) -> f64 {
    let client = reqwest::Client::new();
    fetch_lamports(&client, rpc_url, pubkey).await as f64 / 1_000_000_000.0
}

fn format_price(price: f64) -> String {
    if price.fract() == 0.0 {
        format!("{}", price as u64)
    } else {
        let s = format!("{:.4}", price);
        s.trim_end_matches('0').to_string()
    }
}

fn ensure_local_multi_delegator_program(rpc_url: &str, program_id: &str) -> pay_core::Result<()> {
    if local_program_is_executable(rpc_url, program_id) {
        eprintln!(
            "  {}",
            "multi-delegator program already deployed locally".dimmed()
        );
        return Ok(());
    }

    let repo = std::env::var("PAY_MULTI_DELEGATOR_REPO")
        .unwrap_or_else(|_| "/Users/ludo/Coding/solana-program/multi-delegator".to_string());
    let repo_path = std::path::Path::new(&repo);
    let keypair_path = repo_path.join("keys/multi_delegator-keypair.json");
    let deploy_dir = repo_path.join("target/deploy");
    let deploy_keypair_path = deploy_dir.join("multi_delegator-keypair.json");
    let program_so_path = deploy_dir.join("multi_delegator.so");

    if !keypair_path.exists() {
        return Err(pay_core::Error::Config(format!(
            "multi-delegator keypair not found at {}",
            keypair_path.display()
        )));
    }

    std::fs::create_dir_all(&deploy_dir).map_err(|e| {
        pay_core::Error::Config(format!("failed to create {}: {e}", deploy_dir.display()))
    })?;
    std::fs::copy(&keypair_path, &deploy_keypair_path).map_err(|e| {
        pay_core::Error::Config(format!(
            "failed to copy deploy keypair to {}: {e}",
            deploy_keypair_path.display()
        ))
    })?;

    if !program_so_path.exists() {
        eprintln!("  {}", "building local multi-delegator program...".dimmed());
        let status = ProcessCommand::new("cargo")
            .arg("build-sbf")
            .current_dir(repo_path.join("programs/multi_delegator"))
            .status()
            .map_err(|e| {
                pay_core::Error::Config(format!(
                    "failed to invoke cargo build-sbf for multi-delegator: {e}"
                ))
            })?;
        if !status.success() {
            return Err(pay_core::Error::Config(
                "cargo build-sbf for multi-delegator failed".to_string(),
            ));
        }
    }

    let payer_keypair = localnet_fee_payer_keypair_file()?;
    eprintln!(
        "  {}",
        "deploying multi-delegator program to local Surfpool...".dimmed()
    );
    let status = ProcessCommand::new("solana")
        .arg("program")
        .arg("deploy")
        .arg("--url")
        .arg(rpc_url)
        .arg("--keypair")
        .arg(payer_keypair.path())
        .arg("--fee-payer")
        .arg(payer_keypair.path())
        .arg("--program-id")
        .arg(&deploy_keypair_path)
        .arg(&program_so_path)
        .status()
        .map_err(|e| {
            pay_core::Error::Config(format!(
                "failed to invoke solana program deploy for multi-delegator: {e}"
            ))
        })?;
    if !status.success() {
        return Err(pay_core::Error::Config(
            "solana program deploy for multi-delegator failed".to_string(),
        ));
    }

    if !local_program_is_executable(rpc_url, program_id) {
        return Err(pay_core::Error::Config(format!(
            "multi-delegator program {program_id} still not executable after deploy"
        )));
    }

    eprintln!("  {}", "multi-delegator program deployed locally".green());
    Ok(())
}

fn ensure_local_fiber_program(rpc_url: &str) -> pay_core::Result<solana_pubkey::Pubkey> {
    use std::str::FromStr;

    let repo =
        std::env::var("PAY_FIBER_REPO").unwrap_or_else(|_| "/Users/ludo/Coding/fiber".to_string());
    let repo_path = std::path::Path::new(&repo);
    let deploy_dir = repo_path.join("target/deploy");
    let keypair_path = deploy_dir.join("fiber_native-keypair.json");
    let program_so_path = deploy_dir.join("fiber_native.so");

    if !keypair_path.exists() {
        return Err(pay_core::Error::Config(format!(
            "Fiber program keypair not found at {}",
            keypair_path.display()
        )));
    }

    let keypair_path_str = keypair_path.to_string_lossy();
    let signer = MemorySigner::from_private_key_file(&keypair_path_str).map_err(|e| {
        pay_core::Error::Config(format!("failed to load Fiber program keypair: {e}"))
    })?;
    let program_id =
        solana_pubkey::Pubkey::from_str(&signer.pubkey().to_string()).map_err(|e| {
            pay_core::Error::Config(format!("invalid Fiber program ID from keypair: {e}"))
        })?;

    if local_program_is_executable(rpc_url, &program_id.to_string()) {
        eprintln!("  {}", "Fiber program already deployed locally".dimmed());
        return Ok(program_id);
    }

    if !program_so_path.exists() {
        eprintln!("  {}", "building local Fiber program...".dimmed());
        let status = ProcessCommand::new("cargo")
            .arg("build-sbf")
            .current_dir(repo_path.join("native"))
            .status()
            .map_err(|e| {
                pay_core::Error::Config(format!("failed to invoke cargo build-sbf for Fiber: {e}"))
            })?;
        if !status.success() {
            return Err(pay_core::Error::Config(
                "cargo build-sbf for Fiber failed".to_string(),
            ));
        }
    }

    eprintln!(
        "  {}",
        "deploying Fiber program to local Surfpool...".dimmed()
    );
    let status = ProcessCommand::new("surfpool")
        .arg("run")
        .arg("deployment")
        .arg("--env")
        .arg("localnet")
        .arg("--unsupervised")
        .current_dir(repo_path)
        .status()
        .map_err(|e| {
            pay_core::Error::Config(format!(
                "failed to invoke surfpool deployment runbook for Fiber: {e}"
            ))
        })?;
    if !status.success() {
        return Err(pay_core::Error::Config(
            "surfpool deployment runbook for Fiber failed".to_string(),
        ));
    }

    if !local_program_is_executable(rpc_url, &program_id.to_string()) {
        return Err(pay_core::Error::Config(format!(
            "Fiber program {program_id} still not executable after deploy"
        )));
    }

    eprintln!("  {}", "Fiber program deployed locally".green());
    Ok(program_id)
}

fn local_program_is_executable(rpc_url: &str, program_id: &str) -> bool {
    let output = ProcessCommand::new("curl")
        .arg("-s")
        .arg("-X")
        .arg("POST")
        .arg(rpc_url)
        .arg("-H")
        .arg("Content-Type: application/json")
        .arg("-d")
        .arg(format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getAccountInfo\",\"params\":[\"{program_id}\",{{\"encoding\":\"base64\"}}]}}"
        ))
        .output();

    let Ok(output) = output else {
        return false;
    };
    let body = String::from_utf8_lossy(&output.stdout);
    body.contains("\"executable\":true")
}

fn localnet_fee_payer_keypair_file() -> pay_core::Result<tempfile::NamedTempFile> {
    use std::io::Write;

    let accounts = pay_core::accounts::AccountsFile::load()?;
    let (_, account) = accounts.account_for_network("localnet").ok_or_else(|| {
        pay_core::Error::Config(
            "no localnet account configured in ~/.config/pay/accounts.yml".to_string(),
        )
    })?;
    let bytes = account.ephemeral_keypair_bytes().ok_or_else(|| {
        pay_core::Error::Config(
            "localnet account is not ephemeral or missing secret_key_b58".to_string(),
        )
    })?;

    let mut file = tempfile::NamedTempFile::new().map_err(|e| {
        pay_core::Error::Config(format!("failed to create temp fee payer keypair file: {e}"))
    })?;
    write!(file, "{}", serde_json::to_string(&bytes).unwrap()).map_err(|e| {
        pay_core::Error::Config(format!("failed to write temp fee payer keypair file: {e}"))
    })?;
    Ok(file)
}

/// Emit an OSC 8 clickable hyperlink for terminals that support it.

// ── Gateway verify endpoint ──

#[derive(serde::Deserialize)]
struct GatewayVerifyRequest {
    method: String,
    path: String,
    price: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    authorization: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    request_id: Option<String>,
    #[serde(default)]
    external_id: Option<String>,
    /// JSON-encoded splits array from the gateway (assembled by JS policy).
    #[serde(default)]
    splits_json: Option<String>,
}

#[derive(serde::Serialize)]
struct GatewayVerifyResponse {
    decision: String,
    status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    www_authenticate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    www_authenticate_headers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    challenge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
}

async fn gateway_verify(
    mpps: Vec<Mpp>,
    req: GatewayVerifyRequest,
    pdb: Option<&pay_pdb::PdbState>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use solana_mpp::{format_receipt, format_www_authenticate_many, parse_authorization};

    let auth = req
        .authorization
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    // Parse splits from JSON string (assembled by Apigee JS policy).
    let splits: Vec<solana_mpp::protocol::solana::Split> = req
        .splits_json
        .as_deref()
        .filter(|s| !s.is_empty() && *s != "[]")
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    match auth {
        None => {
            let challenges = match gateway_charge_challenges(&mpps, &req, splits.clone()) {
                Ok(challenges) => challenges,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            };
            let www_auths = format_www_authenticate_many(&challenges).unwrap_or_default();
            let first_www_auth = www_auths.first().cloned().unwrap_or_default();

            // Log 402 challenge to PDB
            if let Some(pdb) = pdb {
                let mut res_headers = std::collections::HashMap::new();
                res_headers.insert("www-authenticate".to_string(), www_auths.join("\n"));
                let entry = pay_pdb::types::LogEntry {
                    id: pdb.next_log_id(),
                    ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    method: req.method.clone(),
                    path: req.path.clone(),
                    status: 402,
                    ms: 0,
                    req_headers: std::collections::HashMap::new(),
                    res_headers,
                    res_body: None,
                    client_ip: "gateway".to_string(),
                };
                pdb.correlation.lock().unwrap().ingest(entry);
            }

            axum::Json(GatewayVerifyResponse {
                decision: "payment_required".to_string(),
                status_code: 402,
                www_authenticate: Some(first_www_auth),
                www_authenticate_headers: Some(www_auths),
                body: Some(serde_json::json!({
                    "error": "payment_required",
                    "endpoint": { "method": req.method, "path": req.path },
                })),
                challenge_id: challenges.first().map(|challenge| challenge.id.clone()),
                external_id: req.external_id,
                receipt: None,
                receipt_status: None,
                receipt_reference: None,
            })
            .into_response()
        }
        Some(auth_value) => {
            let credential = match parse_authorization(auth_value) {
                Ok(c) => c,
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            };
            let mut last_error = None;
            for mpp in &mpps {
                match mpp.verify_credential(&credential).await {
                    Ok(receipt) => {
                        let encoded = format_receipt(&receipt).unwrap_or_default();

                        // Log successful payment to PDB
                        if let Some(pdb) = pdb {
                            let mut req_headers = std::collections::HashMap::new();
                            req_headers.insert(
                                "authorization".to_string(),
                                format!("Payment {}", auth_value),
                            );
                            let entry = pay_pdb::types::LogEntry {
                                id: pdb.next_log_id(),
                                ts: chrono::Utc::now()
                                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                                method: req.method.clone(),
                                path: req.path.clone(),
                                status: 200,
                                ms: 0,
                                req_headers,
                                res_headers: std::collections::HashMap::new(),
                                res_body: None,
                                client_ip: "gateway".to_string(),
                            };
                            pdb.correlation.lock().unwrap().ingest(entry);
                        }

                        return axum::Json(GatewayVerifyResponse {
                            decision: "allow".to_string(),
                            status_code: 200,
                            receipt: Some(encoded),
                            receipt_status: Some(receipt.status.to_string()),
                            receipt_reference: Some(receipt.reference),
                            challenge_id: Some(receipt.challenge_id),
                            external_id: req.external_id,
                            www_authenticate: None,
                            www_authenticate_headers: None,
                            body: Some(serde_json::json!({"pdb_active": pdb.is_some()})),
                        })
                        .into_response();
                    }
                    Err(error) => last_error = Some(error),
                }
            }

            let error = last_error.unwrap_or_else(|| {
                solana_mpp::server::VerificationError::new("MPP not configured")
            });
            let message = pay_core::server::payment::readable_verification_message(&error);
            // Re-issue challenge on failure
            let challenges = gateway_charge_challenges(&mpps, &req, splits).unwrap_or_default();
            let www_auths = format_www_authenticate_many(&challenges).unwrap_or_default();
            axum::Json(GatewayVerifyResponse {
                decision: "payment_required".to_string(),
                status_code: 402,
                www_authenticate: www_auths.first().cloned(),
                www_authenticate_headers: Some(www_auths),
                body: Some(serde_json::json!({
                    "error": "verification_failed",
                    "message": message,
                    "retryable": error.retryable,
                })),
                challenge_id: challenges.first().map(|challenge| challenge.id.clone()),
                external_id: req.external_id,
                receipt: None,
                receipt_status: Some("failed".to_string()),
                receipt_reference: None,
            })
            .into_response()
        }
    }
}

fn gateway_charge_challenges(
    mpps: &[Mpp],
    req: &GatewayVerifyRequest,
    splits: Vec<solana_mpp::protocol::solana::Split>,
) -> Result<Vec<solana_mpp::PaymentChallenge>, solana_mpp::Error> {
    mpps.iter()
        .map(|mpp| {
            mpp.charge_with_options(
                &req.price,
                solana_mpp::server::ChargeOptions {
                    description: req.description.as_deref(),
                    external_id: req.external_id.as_deref(),
                    splits: splits.clone(),
                    ..Default::default()
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        build_pdb_config, explorer_cluster_query, resolve_currency, resolve_operator_currencies,
        validate_browser_rpc_request,
    };

    #[test]
    fn explorer_cluster_query_mainnet_is_empty() {
        // Mainnet is the explorer's default — appending `?cluster=mainnet`
        // would just add noise. The user's expected URL was bare.
        assert_eq!(
            explorer_cluster_query("mainnet", "https://api.mainnet-beta.solana.com"),
            ""
        );
    }

    #[test]
    fn explorer_cluster_query_devnet_picks_devnet() {
        assert_eq!(
            explorer_cluster_query("devnet", "https://api.devnet.solana.com"),
            "?cluster=devnet"
        );
    }

    #[test]
    fn explorer_cluster_query_localnet_uses_custom_with_url_encoding() {
        // The RPC URL must be URL-encoded so the explorer parses it as
        // a single query value.
        assert_eq!(
            explorer_cluster_query("localnet", "https://402.surfnet.dev:8899"),
            "?cluster=custom&customUrl=https%3A%2F%2F402.surfnet.dev%3A8899"
        );
    }

    #[test]
    fn explorer_cluster_query_localnet_with_localhost_rpc() {
        assert_eq!(
            explorer_cluster_query("localnet", "http://localhost:8899"),
            "?cluster=custom&customUrl=http%3A%2F%2Flocalhost%3A8899"
        );
    }

    #[test]
    fn explorer_cluster_query_unknown_network_falls_back_to_empty() {
        // Anything we don't recognize gets the bare URL — let the
        // explorer apply its own default rather than guessing.
        assert_eq!(explorer_cluster_query("foonet", "http://example/"), "");
    }

    #[test]
    fn resolve_operator_currencies_prefers_usd_group() {
        let op: pay_types::metering::OperatorConfig = serde_yml::from_str(
            r#"
currencies:
  usd: ["USDC", "USDT", "CASH"]
"#,
        )
        .unwrap();

        assert_eq!(
            resolve_operator_currencies(Some(&op), "PYUSD"),
            ["USDC", "USDT", "CASH"]
        );
    }

    #[test]
    fn resolve_operator_currencies_falls_back_to_cli_currency() {
        let op: pay_types::metering::OperatorConfig =
            serde_yml::from_str(r#"network: "devnet""#).unwrap();

        assert_eq!(resolve_operator_currencies(Some(&op), "USDC"), ["USDC"]);
    }

    #[test]
    fn operator_config_rejects_removed_currency_field() {
        let err = serde_yml::from_str::<pay_types::metering::OperatorConfig>(r#"currency: "USDT""#)
            .unwrap_err();

        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn resolve_currency_uses_mpp_stablecoin_constants() {
        assert_eq!(
            resolve_currency("USDT", "mainnet").0,
            solana_mpp::protocol::solana::mints::USDT_MAINNET
        );
        assert_eq!(
            resolve_currency("CASH", "mainnet").0,
            solana_mpp::protocol::solana::mints::CASH_MAINNET
        );
    }

    #[test]
    fn browser_rpc_proxy_accepts_payment_page_methods() {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLatestBlockhash",
            "params": [{"commitment": "confirmed"}],
        });

        validate_browser_rpc_request(request.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn browser_rpc_proxy_accepts_surfpool_setup_batch() {
        let request = serde_json::json!([
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "surfnet_setAccount",
                "params": [],
            },
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "surfnet_setTokenAccount",
                "params": [],
            }
        ]);

        validate_browser_rpc_request(request.to_string().as_bytes()).unwrap();
    }

    #[test]
    fn browser_rpc_proxy_rejects_unneeded_rpc_methods() {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [],
        });

        let err = validate_browser_rpc_request(request.to_string().as_bytes()).unwrap_err();
        assert_eq!(err, "Payment RPC method is not allowed.");
    }

    #[test]
    fn pdb_config_uses_real_rpc_url_for_explorer_links() {
        let api: pay_types::metering::ApiSpec = serde_yml::from_str(
            r#"
name: testapi
subdomain: testapi
title: Test API
description: Test API
category: ai_ml
version: v1
routing:
  type: respond
operator:
  recipient: CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY
endpoints:
  - method: GET
    path: v1/data
    resource: data
    metering:
      dimensions:
        - direction: usage
          unit: requests
          scale: 1
          tiers:
            - price_usd: 0.01
"#,
        )
        .unwrap();

        let config = build_pdb_config(
            &api,
            "CXhrFZJLKqjzmP3sjYLcF4dTeXWKCy9e2SXXZ2Yo6MPY",
            "localnet",
            "https://402.surfnet.dev:8899",
        );

        assert_eq!(config["rpcUrl"], "https://402.surfnet.dev:8899");
    }

    // ── resolve_signer (operator.signer in YAML) ───────────────────────────
    //
    // Tests for the SignerConfig variants exposed via `operator.signer` in
    // a provider YAML. Each variant is exercised through
    // `resolve_signer_with_store` so we can inject a `MemoryAccountsStore`
    // and never touch `~/.config/pay/accounts.yml`.

    use super::resolve_signer_with_store;
    use pay_core::accounts::{
        Account, AccountsFile, Keystore as AcctKeystore, MemoryAccountsStore,
    };
    use pay_types::metering::SignerConfig;
    // SolanaSigner trait is brought into scope by the parent module's
    // `use solana_mpp::solana_keychain::SolanaSigner;` so calls like
    // `signer.pubkey()` resolve through the trait method.

    /// A real ed25519 keypair (sk[32] || pk[32]) lifted from the
    /// solana-keychain crate's test fixtures. Stable across runs so
    /// pubkey assertions can pin a known value.
    const VALID_TEST_KEYPAIR_BYTES: [u8; 64] = [
        41, 99, 180, 88, 51, 57, 48, 80, 61, 63, 219, 75, 176, 49, 116, 254, 227, 176, 196, 204,
        122, 47, 166, 133, 155, 252, 217, 0, 253, 17, 49, 143, 47, 94, 121, 167, 195, 136, 72, 22,
        157, 48, 77, 88, 63, 96, 57, 122, 181, 243, 236, 188, 241, 134, 174, 224, 100, 246, 17,
        170, 104, 17, 151, 48,
    ];

    /// Pubkey base58 derived from `VALID_TEST_KEYPAIR_BYTES[32..]` —
    /// pinned so the tests catch unintended drift in the keypair format.
    const VALID_TEST_KEYPAIR_PUBKEY: &str = "4BuiY9QUUfPoAGNJBja3JapAuVWMc9c7in6UCgyC2zPR";

    fn write_test_keypair_file(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("test-key.json");
        let json: Vec<i64> = VALID_TEST_KEYPAIR_BYTES.iter().map(|&b| b as i64).collect();
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        path
    }

    fn ephemeral_account_with_known_pubkey() -> (Account, String) {
        let pubkey = bs58::encode(&VALID_TEST_KEYPAIR_BYTES[32..]).into_string();
        let acct = Account {
            keystore: AcctKeystore::Ephemeral,
            active: false,
            auth_required: Some(false),
            pubkey: Some(pubkey.clone()),
            vault: None,
            account: None,
            path: None,
            secret_key_b58: Some(bs58::encode(&VALID_TEST_KEYPAIR_BYTES[..]).into_string()),
            created_at: Some("2026-04-10T00:00:00Z".to_string()),
        };
        (acct, pubkey)
    }

    // ── File backend ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_signer_file_loads_valid_keypair() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_keypair_file(dir.path());
        let cfg = SignerConfig::File {
            path: path.to_string_lossy().into_owned(),
        };
        let store = MemoryAccountsStore::new();

        let signer = resolve_signer_with_store(&cfg, &store).await.unwrap();
        assert_eq!(
            signer.pubkey().to_string(),
            VALID_TEST_KEYPAIR_PUBKEY,
            "loaded signer's pubkey must match the keypair we wrote"
        );
    }

    #[tokio::test]
    async fn resolve_signer_file_errors_on_missing_path() {
        let cfg = SignerConfig::File {
            path: "/var/folders/sr/this-path-definitely-does-not-exist.json".to_string(),
        };
        let store = MemoryAccountsStore::new();

        let err = match resolve_signer_with_store(&cfg, &store).await {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        // The wrapped error should mention the offending path AND the
        // keygen hint so the user knows what to do next.
        assert!(
            msg.contains("does-not-exist.json"),
            "missing path in error: {msg}"
        );
        assert!(
            msg.contains("solana-keygen new"),
            "missing remediation hint: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_signer_file_errors_on_garbage_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.json");
        std::fs::write(&path, "this is not a keypair").unwrap();
        let cfg = SignerConfig::File {
            path: path.to_string_lossy().into_owned(),
        };
        let store = MemoryAccountsStore::new();

        let err = match resolve_signer_with_store(&cfg, &store).await {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("64 bytes"), "missing length hint: {msg}");
    }

    // ── Account backend ────────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_signer_account_loads_ephemeral_entry() {
        // The most common dev path: a named ephemeral account in
        // accounts.yml. No OS auth prompt fires because the secret is
        // stored inline.
        let mut file = AccountsFile::default();
        let (account, expected_pubkey) = ephemeral_account_with_known_pubkey();
        file.upsert(pay_core::accounts::MAINNET_NETWORK, "test-payer", account);
        let store = MemoryAccountsStore::with_file(file);

        let cfg = SignerConfig::Account {
            name: "test-payer".to_string(),
        };
        let signer = resolve_signer_with_store(&cfg, &store).await.unwrap();
        assert_eq!(signer.pubkey().to_string(), expected_pubkey);
    }

    #[tokio::test]
    async fn resolve_signer_account_errors_on_unknown_name() {
        let store = MemoryAccountsStore::new();
        let cfg = SignerConfig::Account {
            name: "ghost-account".to_string(),
        };

        let err = match resolve_signer_with_store(&cfg, &store).await {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("ghost-account"), "missing account name: {msg}");
        assert!(
            msg.contains("pay account ls"),
            "missing remediation hint: {msg}"
        );
    }

    #[tokio::test]
    async fn resolve_signer_account_errors_on_corrupt_ephemeral_secret() {
        // Account is marked ephemeral but secret_key_b58 isn't valid
        // base58. Should fail with a helpful message naming the account.
        let mut file = AccountsFile::default();
        let bad = Account {
            keystore: AcctKeystore::Ephemeral,
            active: false,
            auth_required: Some(false),
            pubkey: Some("4BuiY9QUUfPoAGNJBja3JapAuVWMc9c7in6UCgyC2zPR".to_string()),
            vault: None,
            account: None,
            path: None,
            // Valid base58 but wrong length (decodes to <64 bytes).
            secret_key_b58: Some("abc".to_string()),
            created_at: Some("2026-04-10T00:00:00Z".to_string()),
        };
        file.upsert(pay_core::accounts::MAINNET_NETWORK, "broken", bad);
        let store = MemoryAccountsStore::with_file(file);

        let cfg = SignerConfig::Account {
            name: "broken".to_string(),
        };
        let err = match resolve_signer_with_store(&cfg, &store).await {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("broken"), "missing account name: {msg}");
    }

    // ── GcpKms backend (build-feature gated) ──────────────────────────────

    #[tokio::test]
    #[cfg(not(feature = "gcp_kms"))]
    async fn resolve_signer_gcp_kms_errors_when_feature_missing() {
        // Without the gcp_kms feature, the GcpKms variant must error
        // with a clear "rebuild with --features gcp_kms" hint AND
        // mention the alternative backends so the user has options.
        let cfg = SignerConfig::GcpKms {
            key_name: "projects/x/locations/y/keyRings/z/cryptoKeys/a/cryptoKeyVersions/1"
                .to_string(),
            pubkey: "4BuiY9QUUfPoAGNJBja3JapAuVWMc9c7in6UCgyC2zPR".to_string(),
        };
        let store = MemoryAccountsStore::new();

        let err = match resolve_signer_with_store(&cfg, &store).await {
            Ok(_) => panic!("expected error, got Ok"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("gcp_kms"), "missing feature name: {msg}");
        assert!(
            msg.contains("backend: account"),
            "missing alt-backend hint: {msg}"
        );
    }
}
