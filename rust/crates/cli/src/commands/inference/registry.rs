//! The user's remote inference gateway registry —
//! `~/.config/pay/inference.yaml`.
//!
//! Each entry is a remote `pay serve inference` gateway registered via
//! `pay inference add <domain-or-ip>`. The provider snapshot (title, models,
//! per-model pricing) is fetched from the gateway's discovery document at
//! add time and cached here, so `pay inference ls` works offline and the
//! `pay claude` picker can label rows without waiting on slow origins.
//! Liveness is always re-checked at pick time — the cache is display
//! metadata, not routing truth.

use serde::{Deserialize, Serialize};

use pay_pdb::types::ProviderSummary;

/// Registry file path (tilde-expanded on load/save).
pub const REGISTRY_PATH: &str = "~/.config/pay/inference.yaml";

/// The discovery document a pay inference gateway serves. The same snapshot
/// the local-gateway path reads on `127.0.0.1:1402`, here fetched from a
/// remote origin.
pub const GATEWAY_CONFIG_PATH: &str = "/__402/pdb/api/config";

/// Schema of `~/.config/pay/inference.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InferenceRegistry {
    #[serde(default)]
    pub gateways: Vec<GatewayEntry>,
}

/// One registered remote gateway plus its cached provider snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayEntry {
    /// Normalized origin, e.g. `http://203.0.113.4:8080` (no trailing slash).
    pub origin: String,
    /// Gateway display title from the discovery document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `sandbox` | `mainnet` — as reported by the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// RFC 3339 timestamp of the last successful snapshot fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refreshed_at: Option<String>,
    /// Cached provider snapshot (same shape the gateway serves live).
    #[serde(default)]
    pub providers: Vec<ProviderSummary>,
}

impl InferenceRegistry {
    /// Load the registry; a missing file is an empty registry.
    pub fn load() -> pay_core::Result<Self> {
        Self::load_from(REGISTRY_PATH)
    }

    pub fn load_from(path: &str) -> pay_core::Result<Self> {
        let expanded = shellexpand::tilde(path).to_string();
        let contents = match std::fs::read_to_string(&expanded) {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(pay_core::Error::Config(format!(
                    "read inference registry {path}: {e}"
                )));
            }
        };
        serde_yml::from_str(&contents)
            .map_err(|e| pay_core::Error::Config(format!("parse inference registry {path}: {e}")))
    }

    /// Save the registry, creating parent directories as needed.
    pub fn save(&self) -> pay_core::Result<()> {
        self.save_to(REGISTRY_PATH)
    }

    pub fn save_to(&self, path: &str) -> pay_core::Result<()> {
        let expanded = shellexpand::tilde(path).to_string();
        if let Some(parent) = std::path::Path::new(&expanded).parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                pay_core::Error::Config(format!("create config dir for {path}: {e}"))
            })?;
        }
        let contents = serde_yml::to_string(self)
            .map_err(|e| pay_core::Error::Config(format!("serialize inference registry: {e}")))?;
        std::fs::write(&expanded, contents)
            .map_err(|e| pay_core::Error::Config(format!("write inference registry {path}: {e}")))
    }

    /// Insert or replace the entry with `entry.origin`. Returns `true` when
    /// an existing entry was replaced.
    pub fn upsert(&mut self, entry: GatewayEntry) -> bool {
        if let Some(existing) = self
            .gateways
            .iter_mut()
            .find(|g| origins_match(&g.origin, &entry.origin))
        {
            *existing = entry;
            return true;
        }
        self.gateways.push(entry);
        false
    }

    /// Remove the entry matching `origin` (scheme-lenient). Returns the
    /// removed entry when one matched.
    pub fn remove(&mut self, origin: &str) -> Option<GatewayEntry> {
        let idx = self
            .gateways
            .iter()
            .position(|g| origins_match(&g.origin, origin))?;
        Some(self.gateways.remove(idx))
    }
}

/// Compare two origins ignoring scheme and trailing slashes, so
/// `pay inference rm 203.0.113.4:8080` matches a stored
/// `http://203.0.113.4:8080`.
pub fn origins_match(a: &str, b: &str) -> bool {
    strip_scheme(a).eq_ignore_ascii_case(strip_scheme(b))
}

fn strip_scheme(origin: &str) -> &str {
    let origin = origin.trim_end_matches('/');
    origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(origin: &str) -> GatewayEntry {
        GatewayEntry {
            origin: origin.to_string(),
            title: Some("Pay Inference".to_string()),
            network: Some("sandbox".to_string()),
            refreshed_at: None,
            providers: Vec::new(),
        }
    }

    #[test]
    fn missing_file_loads_as_empty_registry() {
        let registry =
            InferenceRegistry::load_from("/tmp/definitely-missing/pay-inference.yaml").unwrap();
        assert!(registry.gateways.is_empty());
    }

    #[test]
    fn roundtrips_through_yaml_on_disk() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pay-inference-{}.yaml", std::process::id()));
        let path = path.to_str().unwrap();

        let mut registry = InferenceRegistry::default();
        registry.upsert(GatewayEntry {
            origin: "http://203.0.113.4:8080".to_string(),
            title: Some("Pay Inference".to_string()),
            network: Some("sandbox".to_string()),
            refreshed_at: Some("2026-07-08T00:00:00Z".to_string()),
            providers: vec![ProviderSummary {
                slug: "llama-cpp".to_string(),
                title: "llama.cpp".to_string(),
                base_url: "http://127.0.0.1:8081".to_string(),
                up: true,
                models: vec!["gpt-oss-120b".to_string()],
                version: None,
                color: Some("#f59e0b".to_string()),
                model_pricing: Vec::new(),
            }],
        });
        registry.save_to(path).unwrap();

        let reloaded = InferenceRegistry::load_from(path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(reloaded, registry);
    }

    #[test]
    fn upsert_replaces_scheme_lenient_and_appends_new() {
        let mut registry = InferenceRegistry::default();
        assert!(!registry.upsert(entry("http://203.0.113.4:8080")));
        // Same host+port, different scheme spelling → replace, not append.
        assert!(registry.upsert(entry("203.0.113.4:8080/")));
        assert_eq!(registry.gateways.len(), 1);
        assert!(!registry.upsert(entry("https://other.example.com")));
        assert_eq!(registry.gateways.len(), 2);
    }

    #[test]
    fn remove_matches_without_scheme() {
        let mut registry = InferenceRegistry::default();
        registry.upsert(entry("http://203.0.113.4:8080"));
        assert!(registry.remove("203.0.113.4:8080").is_some());
        assert!(registry.gateways.is_empty());
        assert!(registry.remove("203.0.113.4:8080").is_none());
    }
}
