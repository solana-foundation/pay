//! Pluggable read/write for policies.toml + policy-state.json.

use std::path::PathBuf;
use std::sync::RwLock;

use super::config::PoliciesFile;
use super::state::PolicyState;
use crate::{Error, Result};

const POLICIES_FILE: &str = "~/.config/pay/policies.toml";
const STATE_FILE: &str = "~/.config/pay/policy-state.json";

/// Read/write abstraction. Real impl is [`FilePolicyStore`]; tests use
/// [`MemoryPolicyStore`].
pub trait PolicyStore: Send + Sync {
    fn load_policies(&self) -> Result<PoliciesFile>;
    fn save_policies(&self, file: &PoliciesFile) -> Result<()>;
    fn load_state(&self) -> Result<PolicyState>;
    fn save_state(&self, state: &PolicyState) -> Result<()>;
}

/// On-disk store: TOML for policies, JSON for state. Both written
/// atomically with `0600` permissions.
pub struct FilePolicyStore {
    policies_path: PathBuf,
    state_path: PathBuf,
}

impl FilePolicyStore {
    /// Default paths under `~/.config/pay/`.
    pub fn default_path() -> Self {
        Self {
            policies_path: PathBuf::from(shellexpand::tilde(POLICIES_FILE).into_owned()),
            state_path: PathBuf::from(shellexpand::tilde(STATE_FILE).into_owned()),
        }
    }

    /// Explicit paths (used by tests and non-default deployments).
    pub fn at(policies_path: PathBuf, state_path: PathBuf) -> Self {
        Self {
            policies_path,
            state_path,
        }
    }

    pub fn policies_path(&self) -> &PathBuf {
        &self.policies_path
    }

    pub fn state_path(&self) -> &PathBuf {
        &self.state_path
    }
}

impl PolicyStore for FilePolicyStore {
    fn load_policies(&self) -> Result<PoliciesFile> {
        if !self.policies_path.exists() {
            return Ok(PoliciesFile::default());
        }
        let raw = std::fs::read_to_string(&self.policies_path).map_err(|e| {
            Error::Config(format!(
                "Failed to read {}: {e}",
                self.policies_path.display()
            ))
        })?;
        if raw.trim().is_empty() {
            return Ok(PoliciesFile::default());
        }
        toml::from_str(&raw)
            .map_err(|e| Error::Config(format!("Invalid {}: {e}", self.policies_path.display())))
    }

    fn save_policies(&self, file: &PoliciesFile) -> Result<()> {
        let serialized = toml::to_string_pretty(file)
            .map_err(|e| Error::Config(format!("TOML serialize: {e}")))?;
        atomic_write_private(&self.policies_path, serialized.as_bytes())
    }

    fn load_state(&self) -> Result<PolicyState> {
        if !self.state_path.exists() {
            return Ok(PolicyState::default());
        }
        let raw = std::fs::read_to_string(&self.state_path).map_err(|e| {
            Error::Config(format!("Failed to read {}: {e}", self.state_path.display()))
        })?;
        if raw.trim().is_empty() {
            return Ok(PolicyState::default());
        }
        serde_json::from_str(&raw)
            .map_err(|e| Error::Config(format!("Invalid {}: {e}", self.state_path.display())))
    }

    fn save_state(&self, state: &PolicyState) -> Result<()> {
        let serialized = serde_json::to_string_pretty(state)
            .map_err(|e| Error::Config(format!("JSON serialize: {e}")))?;
        atomic_write_private(&self.state_path, serialized.as_bytes())
    }
}

/// Write `data` to `path` atomically (temp file + rename) with `0600`
/// permissions on Unix. Creates parent directories with `0700`.
fn atomic_write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        Error::Config(format!("Path {} has no parent dir", path.display()))
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|e| Error::Config(format!("Failed to create dir {}: {e}", parent.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|e| Error::Config(format!("Failed to create temp file: {e}")))?;
    {
        use std::io::Write;
        tmp.write_all(data)
            .map_err(|e| Error::Config(format!("Failed to write temp file: {e}")))?;
        tmp.flush()
            .map_err(|e| Error::Config(format!("Failed to flush temp file: {e}")))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600));
    }

    tmp.persist(path)
        .map_err(|e| Error::Config(format!("Failed to rename temp file into place: {e}")))?;
    Ok(())
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryPolicyStore {
    policies: RwLock<PoliciesFile>,
    state: RwLock<PolicyState>,
}

impl MemoryPolicyStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_policies(policies: PoliciesFile) -> Self {
        Self {
            policies: RwLock::new(policies),
            state: RwLock::new(PolicyState::default()),
        }
    }

    pub fn snapshot_state(&self) -> PolicyState {
        self.state.read().unwrap().clone()
    }
}

impl PolicyStore for MemoryPolicyStore {
    fn load_policies(&self) -> Result<PoliciesFile> {
        Ok(self.policies.read().unwrap().clone())
    }
    fn save_policies(&self, file: &PoliciesFile) -> Result<()> {
        *self.policies.write().unwrap() = file.clone();
        Ok(())
    }
    fn load_state(&self) -> Result<PolicyState> {
        Ok(self.state.read().unwrap().clone())
    }
    fn save_state(&self, state: &PolicyState) -> Result<()> {
        *self.state.write().unwrap() = state.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::Policy;
    use super::*;
    use chrono::{DateTime, Utc};

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn sample() -> Policy {
        Policy {
            name: "test".to_string(),
            max_per_tx: 100_000,
            daily_cap: 1_000_000,
            allowed_recipients: vec!["R1".to_string()],
            allowed_origins: vec!["api.example.com".to_string()],
            expires_at: Some(t("2027-01-01T00:00:00Z")),
            paused: false,
            created_at: t("2026-01-01T00:00:00Z"),
        }
    }

    #[test]
    fn file_store_roundtrip_policies_and_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilePolicyStore::at(
            dir.path().join("policies.toml"),
            dir.path().join("state.json"),
        );

        // Empty load returns defaults.
        assert!(store.load_policies().unwrap().policies.is_empty());
        assert!(store.load_state().unwrap().per_policy.is_empty());

        // Save and reload policies.
        let mut policies = PoliciesFile::default();
        policies.upsert(sample());
        policies.set_default("test").unwrap();
        store.save_policies(&policies).unwrap();
        let reloaded = store.load_policies().unwrap();
        assert_eq!(reloaded.default.as_deref(), Some("test"));
        assert_eq!(reloaded.policies.get("test"), policies.policies.get("test"));

        // Save and reload state.
        let mut state = PolicyState::default();
        let entry = state.entry_mut("test");
        entry.spent_today = 250_000;
        entry.day_reset_ts = Some(t("2026-05-01T00:00:00Z"));
        store.save_state(&state).unwrap();
        let reloaded_state = store.load_state().unwrap();
        assert_eq!(
            reloaded_state.per_policy.get("test").unwrap().spent_today,
            250_000
        );
    }

    #[test]
    fn file_store_atomic_write_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let store = FilePolicyStore::at(
            nested.join("policies.toml"),
            nested.join("state.json"),
        );

        store.save_policies(&PoliciesFile::default()).unwrap();
        assert!(nested.join("policies.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_store_writes_with_0600_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FilePolicyStore::at(
            dir.path().join("policies.toml"),
            dir.path().join("state.json"),
        );
        store.save_policies(&PoliciesFile::default()).unwrap();
        let mode = std::fs::metadata(dir.path().join("policies.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryPolicyStore::new();
        let mut policies = PoliciesFile::default();
        policies.upsert(sample());
        store.save_policies(&policies).unwrap();
        assert_eq!(
            store
                .load_policies()
                .unwrap()
                .policies
                .keys()
                .collect::<Vec<_>>(),
            vec![&"test".to_string()]
        );
    }
}
