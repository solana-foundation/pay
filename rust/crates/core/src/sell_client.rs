//! The seller's side of `sell_inference`: pay-connect's `/v1/endpoints` API
//! and the local record of each endpoint this machine created.
//!
//! Both the `sell_inference` MCP tool and `pay sell` use this. The record
//! keeps the owner token out of transcripts: the tool creates the endpoint,
//! writes the record, and `pay sell serve <id>` reads the token from it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

const URL_ENV: &str = "PAY_CONNECT_URL";
const LOCAL_ENV: &str = "PAY_CONNECT_LOCAL";
const DEFAULT_URL: &str = "https://connect.pay.sh";
const LOCAL_URL: &str = "http://127.0.0.1:8402";
/// Overrides where records live (tests).
const DIR_ENV: &str = "PAY_SELL_DIR";
const DEFAULT_DIR: &str = "~/.config/pay/sell";

/// pay-connect base URL: `PAY_CONNECT_URL` when set, else the local server
/// when `PAY_CONNECT_LOCAL` is on, else production.
pub fn default_connect_url() -> String {
    if let Some(url) = std::env::var(URL_ENV)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
    {
        return url;
    }
    let local = std::env::var(LOCAL_ENV)
        .ok()
        .is_some_and(|v| !matches!(v.trim(), "" | "0" | "false" | "FALSE" | "no"));
    if local {
        LOCAL_URL.to_string()
    } else {
        DEFAULT_URL.to_string()
    }
}

/// `/v1/endpoints` on one pay-connect.
pub struct EndpointsApi {
    base: String,
    http: reqwest::Client,
}

impl EndpointsApi {
    pub fn new(connect_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(format!("pay/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))?;
        Ok(Self {
            base: connect_url.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    pub fn connect_url(&self) -> &str {
        &self.base
    }

    /// Create an endpoint with a creator bearer (a linked CLI or connector
    /// token). The view carries the owner token exactly once.
    pub async fn create(&self, creator_token: &str, body: &Value) -> Result<Value> {
        self.send(
            self.http
                .post(format!("{}/v1/endpoints", self.base))
                .bearer_auth(creator_token)
                .json(body),
        )
        .await
    }

    pub async fn view(&self, owner_token: &str, id: &str) -> Result<Value> {
        self.send(
            self.http
                .get(format!("{}/v1/endpoints/{id}", self.base))
                .bearer_auth(owner_token),
        )
        .await
    }

    pub async fn reprice(&self, owner_token: &str, id: &str, pricing: &Value) -> Result<Value> {
        self.send(
            self.http
                .patch(format!("{}/v1/endpoints/{id}", self.base))
                .bearer_auth(owner_token)
                .json(&serde_json::json!({ "pricing": pricing })),
        )
        .await
    }

    pub async fn delete(&self, owner_token: &str, id: &str) -> Result<()> {
        self.send(
            self.http
                .delete(format!("{}/v1/endpoints/{id}", self.base))
                .bearer_auth(owner_token),
        )
        .await
        .map(|_| ())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value> {
        let response = request
            .send()
            .await
            .map_err(|e| Error::Config(format!("pay-connect at {} unreachable: {e}", self.base)))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| text.trim().to_string());
            return Err(Error::Config(format!(
                "pay-connect answered {status}: {message}"
            )));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("pay-connect answered oddly: {e}")))
    }
}

/// What this machine remembers about an endpoint it created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SellRecord {
    pub id: String,
    pub connect_url: String,
    /// Whoever holds it drains the queue. Stored with owner-only permissions.
    pub owner_token: String,
    pub model: String,
    pub base_url: String,
    pub chat_completions_url: String,
    pub recipient: String,
    pub pricing: Value,
    /// Selling stops once this much has been earned, in USD.
    #[serde(default)]
    pub earn_cap_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    pub created_at: String,
}

impl SellRecord {
    /// Build from a creation view, which must still carry `owner_token`.
    pub fn from_created(connect_url: &str, view: &Value) -> Result<Self> {
        let field = |key: &str| {
            view.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| Error::Config(format!("endpoint view is missing `{key}`")))
        };
        Ok(Self {
            id: field("id")?,
            connect_url: connect_url.trim_end_matches('/').to_string(),
            owner_token: field("owner_token")?,
            model: field("model")?,
            base_url: field("base_url")?,
            chat_completions_url: field("chat_completions_url")?,
            recipient: field("recipient")?,
            pricing: view.get("pricing").cloned().unwrap_or(Value::Null),
            earn_cap_usd: view
                .get("earn_cap_usd")
                .and_then(Value::as_f64)
                .unwrap_or_default(),
            harness: None,
            cwd: None,
            worker_pid: None,
            log_path: None,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        })
    }

    pub fn dir() -> PathBuf {
        let raw = std::env::var(DIR_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_DIR.to_string());
        PathBuf::from(shellexpand::tilde(&raw).into_owned())
    }

    pub fn path(id: &str) -> PathBuf {
        Self::dir().join(format!("{id}.json"))
    }

    /// Where the worker's output goes.
    pub fn log_path_for(id: &str) -> PathBuf {
        Self::dir().join(format!("{id}.log"))
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::dir();
        std::fs::create_dir_all(&dir)?;
        let path = Self::path(&self.id);
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn load(id: &str) -> Result<Option<Self>> {
        let path = Self::path(id);
        if !path.exists() {
            return Ok(None);
        }
        let json = std::fs::read(&path)?;
        serde_json::from_slice(&json)
            .map(Some)
            .map_err(|e| Error::Config(format!("corrupt sell record {}: {e}", path.display())))
    }

    pub fn list() -> Result<Vec<Self>> {
        let dir = Self::dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Ok(json) = std::fs::read(&path)
                && let Ok(record) = serde_json::from_slice::<Self>(&json)
            {
                records.push(record);
            }
        }
        records.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(records)
    }

    pub fn remove(id: &str) -> Result<()> {
        for path in [Self::path(id), Self::log_path_for(id)] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn view() -> Value {
        json!({
            "id": "e1", "owner_token": "pso_x", "model": "agent",
            "base_url": "https://connect.test/endpoints/e1/v1",
            "chat_completions_url": "https://connect.test/endpoints/e1/v1/chat/completions",
            "recipient": "Cs2zdfUNonRdRGsiZUQQLdTxzxVvJZmgiX2mpLYKuEqP",
            "pricing": { "per_request_usd": 0.02 },
        })
    }

    #[test]
    fn records_round_trip_with_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        // Env is process-global; this test owns PAY_SELL_DIR.
        unsafe { std::env::set_var(DIR_ENV, dir.path()) };
        let mut record = SellRecord::from_created("https://connect.test/", &view()).unwrap();
        record.harness = Some("echo".into());
        record.worker_pid = Some(4242);
        record.save().unwrap();

        let loaded = SellRecord::load("e1").unwrap().unwrap();
        assert_eq!(loaded, record);
        assert_eq!(loaded.connect_url, "https://connect.test");
        assert_eq!(SellRecord::list().unwrap().len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(SellRecord::path("e1"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(SellRecord::load("nope").unwrap().is_none());
        SellRecord::remove("e1").unwrap();
        assert!(SellRecord::load("e1").unwrap().is_none());
        SellRecord::remove("e1").unwrap();
        unsafe { std::env::remove_var(DIR_ENV) };
    }

    #[test]
    fn a_view_without_the_owner_token_is_refused() {
        let mut view = view();
        view.as_object_mut().unwrap().remove("owner_token");
        let error = SellRecord::from_created("https://connect.test", &view).unwrap_err();
        assert!(error.to_string().contains("owner_token"), "{error}");
    }
}
