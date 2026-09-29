//! `sell_inference`: earn stablecoins by selling this agent's inference.
//!
//! The tool creates a paid OpenAI-compatible endpoint on pay-connect, paid
//! to the active Pay account, and starts `pay sell serve` on this machine
//! to answer its requests with a local agent. It is the counterpart of
//! `topup`: when the balance is empty, the user can deposit or earn.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use pay_core::sell_client::{EndpointsApi, SellRecord, default_connect_url};
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::service::{Peer, RoleServer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::context::CallScope;

/// Creator bearer for pay-connect, when the CLI is not linked.
const CREATOR_TOKEN_ENV: &str = "PAY_CONNECT_TOKEN";
const PAYCONNECT_PROVIDER: &str = "payconnect";
const DEFAULT_MODEL: &str = "pay-agent";

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Create an endpoint and start serving it.
    #[default]
    Create,
    /// Queue depth, worker liveness and pricing of an endpoint.
    Status,
    /// Change the pricing of an endpoint.
    Reprice,
    /// Stop the worker and delete the endpoint.
    Stop,
}

/// The local agent that answers requests.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    #[default]
    Claude,
    Codex,
    Goose,
    /// No agent: echoes the request. Only for testing the wiring.
    Echo,
}

impl Harness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Goose => "goose",
            Self::Echo => "echo",
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct Params {
    /// What to do. Defaults to create.
    #[serde(default)]
    pub action: Action,
    /// Flat price per request in USD, e.g. 0.02. Offers every payment scheme.
    /// Either this or the per-token prices is required to create.
    #[serde(default)]
    pub price_per_request_usd: Option<f64>,
    /// USD per 1M input tokens (with price_per_1m_output_tokens_usd and
    /// max_usd_per_request). Tokens are estimated from characters.
    #[serde(default)]
    pub price_per_1m_input_tokens_usd: Option<f64>,
    /// USD per 1M output tokens.
    #[serde(default)]
    pub price_per_1m_output_tokens_usd: Option<f64>,
    /// Most one request may cost under per-token pricing, in USD.
    #[serde(default)]
    pub max_usd_per_request: Option<f64>,
    /// Model id buyers put in `model`. Defaults to "pay-agent".
    #[serde(default)]
    pub model: Option<String>,
    /// Which local agent answers. Defaults to claude.
    #[serde(default)]
    pub harness: Option<Harness>,
    /// Directory the serving agent works in. Defaults to the current directory.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Where buyers pay (base58). Defaults to the active Pay account.
    #[serde(default)]
    pub recipient: Option<String>,
    /// Network slug. Defaults to mainnet.
    #[serde(default)]
    pub network: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Idle seconds before a buyer's session channel closes and settles to
    /// the recipient. Defaults to ten minutes.
    #[serde(default)]
    pub session_idle_close_secs: Option<u32>,
    /// Endpoint to act on for status, reprice and stop. Defaults to the
    /// most recently created one on this machine.
    #[serde(default)]
    pub endpoint_id: Option<String>,
}

pub async fn run(
    params: Params,
    peer: Option<Peer<RoleServer>>,
    scope: &CallScope,
) -> Result<CallToolResult, rmcp::ErrorData> {
    if !scope.body_files {
        return Ok(super::tool_error(
            "sell_inference serves inference from the machine the agent runs on, and this \
             hosted session has none. Run `pay mcp` locally to sell inference.",
        ));
    }
    let outcome = match params.action {
        Action::Create => create(params, peer, scope).await,
        Action::Status => status(params).await,
        Action::Reprice => reprice(params).await,
        Action::Stop => stop(params).await,
    };
    Ok(match outcome {
        Ok(text) => CallToolResult::success(vec![rmcp::model::Content::text(text)]),
        Err(message) => super::tool_error(message),
    })
}

/// `{"per_request_usd": ..}` or `{"per_token": {..}}` from the flat params.
fn pricing_body(params: &Params) -> Result<Value, String> {
    match (
        params.price_per_request_usd,
        params.price_per_1m_input_tokens_usd,
        params.price_per_1m_output_tokens_usd,
    ) {
        (Some(usd), None, None) => Ok(json!({ "per_request_usd": usd })),
        (None, Some(input), Some(output)) => {
            let max_usd = params.max_usd_per_request.ok_or(
                "per-token pricing needs max_usd_per_request, the most one request may cost",
            )?;
            Ok(json!({ "per_token": {
                "default": { "in": input, "out": output },
                "max_usd": max_usd,
            } }))
        }
        (None, None, None) => Err(
            "set a price: price_per_request_usd for a flat price, or both per-1M-token prices \
             with max_usd_per_request"
                .to_string(),
        ),
        _ => Err(
            "pick one pricing: price_per_request_usd alone, or both per-1M-token prices"
                .to_string(),
        ),
    }
}

/// The active account's address on `network`: where earnings land.
fn default_recipient(scope: &CallScope, network: &str) -> Result<String, String> {
    let accounts = scope
        .accounts
        .load()
        .map_err(|e| format!("Failed to load Pay accounts: {e}"))?;
    let selected = match scope.account_override.as_deref() {
        Some(name) => accounts.named_account_for_network(network, name),
        None => accounts.account_for_network(network).map(|(_, a)| a),
    };
    selected
        .and_then(|account| account.pubkey.clone())
        .ok_or_else(|| {
            format!("No Pay account on {network} to receive earnings. Run `pay setup` first.")
        })
}

/// A bearer that may create endpoints: `PAY_CONNECT_TOKEN`, else the token
/// of a linked `payconnect` account (gated like any credential read).
fn creator_token(scope: &CallScope, peer: Option<&Peer<RoleServer>>) -> Result<String, String> {
    if let Some(token) = std::env::var(CREATOR_TOKEN_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Ok(token);
    }
    let accounts = scope
        .accounts
        .load()
        .map_err(|e| format!("Failed to load Pay accounts: {e}"))?;
    let linked = accounts.accounts.iter().find_map(|(network, named)| {
        named
            .iter()
            .find(|(_, a)| a.provider.as_deref() == Some(PAYCONNECT_PROVIDER))
            .map(|(name, account)| (network.clone(), name.clone(), account.clone()))
    });
    let Some((network, name, account)) = linked else {
        return Err(format!(
            "pay-connect does not know this machine yet. Link it with `pay setup --backend \
             connect` (a browser sign-in), or set {CREATOR_TOKEN_ENV} to a connector token."
        ));
    };
    let gated = account.auth_required_for_network(&network);
    let intent = pay_core::keystore::AuthIntent::use_account(
        "create a sell_inference endpoint on pay-connect",
    )
    .with_account_context(&name);
    let credentials = scope
        .accounts
        .credential_source()
        .load(
            &name,
            PAYCONNECT_PROVIDER,
            pay_core::backend::Gate::for_policy(gated, scope.auth_override(peer)),
            &intent,
        )
        .map_err(|e| format!("Could not read the pay-connect token of `{name}`: {e}"))?;
    credentials
        .get(pay_core::remote::payconnect::API_TOKEN_FIELD)
        .cloned()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| format!("Account `{name}` has no pay-connect token stored."))
}

async fn create(
    params: Params,
    peer: Option<Peer<RoleServer>>,
    scope: &CallScope,
) -> Result<String, String> {
    let pricing = pricing_body(&params)?;
    let network = scope
        .network_override
        .clone()
        .or(params.network.clone())
        .unwrap_or_else(|| pay_core::accounts::MAINNET_NETWORK.to_string());
    let (recipient, recipient_note) = match params.recipient.clone() {
        Some(recipient) => (recipient, "as requested"),
        None => (
            default_recipient(scope, &network)?,
            "the active Pay account; watch it with get_balance",
        ),
    };
    let harness = params.harness.unwrap_or_default();
    let cwd = match params.cwd.clone() {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().map_err(|e| format!("current directory: {e}"))?,
    };
    let cwd = std::fs::canonicalize(&cwd)
        .map_err(|e| format!("cwd `{}` is not usable: {e}", cwd.display()))?;
    let model = params
        .model
        .clone()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let token = creator_token(scope, peer.as_ref())?;

    let api = EndpointsApi::new(default_connect_url()).map_err(|e| e.to_string())?;
    let body = json!({
        "pricing": pricing,
        "model": model,
        "title": params.title,
        "description": params.description,
        "recipient": recipient,
        "network": network,
        "session_idle_close_secs": params.session_idle_close_secs,
    });
    let view = api.create(&token, &body).await.map_err(|e| e.to_string())?;
    let mut record =
        SellRecord::from_created(api.connect_url(), &view).map_err(|e| e.to_string())?;
    record.harness = Some(harness.as_str().to_string());
    record.cwd = Some(cwd.to_string_lossy().to_string());
    record
        .save()
        .map_err(|e| format!("could not save the endpoint record: {e}"))?;

    let spawn = spawn_worker(&record, harness, &cwd);
    match spawn {
        Ok((pid, log)) => {
            record.worker_pid = Some(pid);
            record.log_path = Some(log.to_string_lossy().to_string());
            record
                .save()
                .map_err(|e| format!("could not save the endpoint record: {e}"))?;
        }
        Err(error) => {
            return Ok(format!(
                "Endpoint {} was created at {} but the worker did not start: {error}\n\
                 Start it by hand from the directory the agent should work in:\n  \
                 pay sell serve {} --harness {}\n\
                 Until it runs, buyers get 503 and are not charged.",
                record.id,
                record.chat_completions_url,
                record.id,
                harness.as_str()
            ));
        }
    }

    let schemes = view["schemes"]
        .as_array()
        .map(|s| {
            s.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Ok(format!(
        "Selling inference.\n\
         endpoint_id: {id}\n\
         base_url: {base} (OpenAI-compatible; buyers set model \"{model}\")\n\
         price: {price}\n\
         payment schemes: {schemes}\n\
         paid to: {recipient} ({recipient_note})\n\
         worker: pay sell serve, pid {pid}, harness {harness}, cwd {cwd}\n\
         log: {log}\n\n\
         Requests are answered by an agent running here, so keep this machine on while \
         serving. Buyers are charged only for answered requests. Share the base_url and \
         model with buyers; stop with sell_inference {{action: \"stop\"}}.",
        id = record.id,
        base = record.base_url,
        model = record.model,
        price = describe_pricing(&record.pricing),
        recipient = record.recipient,
        pid = record.worker_pid.unwrap_or_default(),
        harness = harness.as_str(),
        cwd = cwd.display(),
        log = record.log_path.clone().unwrap_or_default(),
    ))
}

/// Start `pay sell serve` detached, logging to the record's log file.
fn spawn_worker(
    record: &SellRecord,
    harness: Harness,
    cwd: &std::path::Path,
) -> Result<(u32, PathBuf), String> {
    let exe = std::env::current_exe().map_err(|e| format!("locate the pay binary: {e}"))?;
    let log_path = SellRecord::log_path_for(&record.id);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("open {}: {e}", log_path.display()))?;
    let mut command = Command::new(exe);
    command
        .args([
            "sell",
            "serve",
            &record.id,
            "--harness",
            harness.as_str(),
            "--connect-url",
            &record.connect_url,
            "--cwd",
        ])
        .arg(cwd)
        .env("PAY_SELL_OWNER_TOKEN", &record.owner_token)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: the worker outlives this MCP session and
        // is not taken down by the host's Ctrl-C.
        command.process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("spawn pay sell serve: {e}"))?;
    Ok((child.id(), log_path))
}

fn describe_pricing(pricing: &Value) -> String {
    if let Some(usd) = pricing.get("per_request_usd").and_then(Value::as_f64) {
        return format!("${usd} per request");
    }
    if let Some(per_token) = pricing.get("per_token") {
        let rate = |k: &str| {
            per_token
                .get("default")
                .and_then(|d| d.get(k))
                .and_then(Value::as_f64)
                .unwrap_or_default()
        };
        return format!(
            "${} per 1M input tokens, ${} per 1M output tokens, at most ${} per request",
            rate("in"),
            rate("out"),
            per_token
                .get("max_usd")
                .and_then(Value::as_f64)
                .unwrap_or_default()
        );
    }
    pricing.to_string()
}

fn record_for(params: &Params) -> Result<SellRecord, String> {
    match params.endpoint_id.as_deref() {
        Some(id) => SellRecord::load(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("No endpoint {id} was created from this machine.")),
        None => SellRecord::list()
            .map_err(|e| e.to_string())?
            .pop()
            .ok_or_else(|| "No endpoint has been created from this machine.".to_string()),
    }
}

#[cfg(unix)]
fn worker_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(unix))]
fn worker_alive(_pid: u32) -> bool {
    false
}

async fn status(params: Params) -> Result<String, String> {
    let record = record_for(&params)?;
    let api = EndpointsApi::new(&record.connect_url).map_err(|e| e.to_string())?;
    let view = api
        .view(&record.owner_token, &record.id)
        .await
        .map_err(|e| e.to_string())?;
    let worker = match record.worker_pid {
        Some(pid) if worker_alive(pid) => format!("running (pid {pid})"),
        Some(pid) => format!(
            "not running (pid {pid} is gone; restart with `pay sell serve {} --harness {}`)",
            record.id,
            record.harness.as_deref().unwrap_or("claude")
        ),
        None => "not started".to_string(),
    };
    Ok(format!(
        "endpoint_id: {}\nbase_url: {}\nmodel: {}\nprice: {}\npaid to: {}\nqueue: {} waiting, {} being answered\nworker: {}\nlog: {}",
        record.id,
        record.base_url,
        record.model,
        describe_pricing(
            &view
                .get("pricing")
                .cloned()
                .unwrap_or(record.pricing.clone())
        ),
        record.recipient,
        view["queue"]["waiting"].as_u64().unwrap_or(0),
        view["queue"]["claimed"].as_u64().unwrap_or(0),
        worker,
        record.log_path.clone().unwrap_or_default(),
    ))
}

async fn reprice(params: Params) -> Result<String, String> {
    let mut record = record_for(&params)?;
    let pricing = pricing_body(&params)?;
    let api = EndpointsApi::new(&record.connect_url).map_err(|e| e.to_string())?;
    let view = api
        .reprice(&record.owner_token, &record.id, &pricing)
        .await
        .map_err(|e| e.to_string())?;
    record.pricing = view.get("pricing").cloned().unwrap_or(pricing);
    record.save().map_err(|e| e.to_string())?;
    Ok(format!(
        "Endpoint {} now costs {}; schemes: {}.",
        record.id,
        describe_pricing(&record.pricing),
        view["schemes"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    ))
}

async fn stop(params: Params) -> Result<String, String> {
    let record = record_for(&params)?;
    let mut notes = Vec::new();
    #[cfg(unix)]
    if let Some(pid) = record.worker_pid
        && worker_alive(pid)
    {
        let _ = Command::new("kill")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        notes.push(format!("worker pid {pid} stopped"));
    }
    let api = EndpointsApi::new(&record.connect_url).map_err(|e| e.to_string())?;
    match api.delete(&record.owner_token, &record.id).await {
        Ok(()) => notes.push("endpoint deleted".to_string()),
        Err(e) => notes.push(format!("endpoint not deleted: {e}")),
    }
    SellRecord::remove(&record.id).map_err(|e| e.to_string())?;
    Ok(format!(
        "Stopped selling on {}: {}. Earnings already settled stay in {}.",
        record.id,
        notes.join(", "),
        record.recipient
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pricing_is_flat_or_per_token_never_both() {
        let flat = Params {
            price_per_request_usd: Some(0.02),
            ..Default::default()
        };
        assert_eq!(
            pricing_body(&flat).unwrap(),
            json!({ "per_request_usd": 0.02 })
        );

        let tokens = Params {
            price_per_1m_input_tokens_usd: Some(0.1),
            price_per_1m_output_tokens_usd: Some(0.3),
            max_usd_per_request: Some(0.25),
            ..Default::default()
        };
        assert_eq!(
            pricing_body(&tokens).unwrap(),
            json!({ "per_token": { "default": { "in": 0.1, "out": 0.3 }, "max_usd": 0.25 } })
        );

        let missing_cap = Params {
            price_per_1m_input_tokens_usd: Some(0.1),
            price_per_1m_output_tokens_usd: Some(0.3),
            ..Default::default()
        };
        assert!(
            pricing_body(&missing_cap)
                .unwrap_err()
                .contains("max_usd_per_request")
        );
        assert!(
            pricing_body(&Params::default())
                .unwrap_err()
                .contains("set a price")
        );
        let both = Params {
            price_per_request_usd: Some(0.02),
            price_per_1m_input_tokens_usd: Some(0.1),
            ..Default::default()
        };
        assert!(pricing_body(&both).unwrap_err().contains("pick one"));
    }

    #[test]
    fn pricing_is_described_for_people() {
        assert_eq!(
            describe_pricing(&json!({ "per_request_usd": 0.02 })),
            "$0.02 per request"
        );
        assert_eq!(
            describe_pricing(
                &json!({ "per_token": { "default": { "in": 0.1, "out": 0.3 }, "max_usd": 0.25 } })
            ),
            "$0.1 per 1M input tokens, $0.3 per 1M output tokens, at most $0.25 per request"
        );
    }
}
