//! Per-provider pin store — overlay layer that shadows the canonical
//! catalog with a single provider fetched from a specific PR/branch/SHA.
//!
//! Layout (under `~/.config/pay/skills/overlay/`):
//!
//! ```text
//! overlay/
//! └── <fqn>/                 (e.g. venice/ai/)
//!     ├── .pin.json          (PinManifest — anchor + provenance)
//!     ├── PAY.md             (the provider's own files, fetched as-is)
//!     └── …
//! ```
//!
//! Writes are atomic: a staging directory is built under
//! `overlay/.staging-<rand>/`, then renamed into place. A partial fetch
//! leaves the previous pin (if any) untouched. The pin manifest stores
//! sha256 of every file installed so drift can be detected later.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// File name for the per-pin manifest. Hidden so it doesn't collide with
/// any actual provider file (which is typically `PAY.md`, `openapi.json`,
/// etc.).
const PIN_MANIFEST_FILE: &str = ".pin.json";

const OVERLAY_DIR: &str = "~/.config/pay/skills/overlay";

/// Total bytes a single pin is allowed to occupy. Guard against runaway
/// repository content blowing up the user's config dir.
pub const MAX_PIN_BYTES: u64 = 10 * 1024 * 1024;

/// What the pin is anchored to. The SHA is the reproducibility anchor;
/// the kind-specific payload is the human label preserved for display
/// and refresh semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PinAnchor {
    /// Anchored to a pull request. `pr` is the PR number; `head_ref` is
    /// the head branch name at the time of install. The PR head SHA can
    /// move between installs — that's a refresh case.
    Pr { pr: u32, head_ref: String },
    /// Anchored to a branch. The branch can move; reinstall re-resolves.
    Branch { ref_name: String },
    /// Anchored to a specific commit SHA. Immutable by definition.
    Sha,
}

/// One file installed by a pin. The sha256 lets us detect drift on a
/// future `pay skills verify` (out of scope today, but cheap to record).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PinFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

/// The per-pin manifest written as `.pin.json` next to the provider's
/// files. Cross-pin index lives only in the filesystem layout — no
/// global file to drift out of sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinManifest {
    /// Provider FQN — same path that appears under `providers/` upstream
    /// (e.g. `venice/ai`).
    pub fqn: String,
    /// The base catalog repo this pin was sourced against. Defaults to
    /// `solana-foundation/pay-skills`.
    pub source_repo: String,
    /// The actual repo the files were fetched from — for PRs from forks
    /// this is the fork repo, not `source_repo`.
    pub head_repo: String,
    /// Anchor kind + label.
    pub anchor: PinAnchor,
    /// Commit SHA resolved at install time. Immutable identity for the
    /// fetched content; refreshing a PR pin updates this and rewrites
    /// the directory.
    pub sha: String,
    /// RFC 3339 timestamp.
    pub installed_at: String,
    /// True when the upstream PR was merged at install time. Hint only —
    /// `pay skills update --prune-merged` re-resolves before acting.
    pub merged: bool,
    /// Files installed (relative to the pin directory, ordered).
    pub files: Vec<PinFile>,
}

impl PinManifest {
    /// Human-readable short SHA for display (first 7 chars).
    pub fn short_sha(&self) -> &str {
        if self.sha.len() >= 7 {
            &self.sha[..7]
        } else {
            &self.sha
        }
    }

    /// Display label for the anchor — e.g. "PR 137" / "branch X" / "sha abc1234".
    pub fn anchor_label(&self) -> String {
        match &self.anchor {
            PinAnchor::Pr { pr, .. } => format!("PR {pr}"),
            PinAnchor::Branch { ref_name } => format!("branch {ref_name}"),
            PinAnchor::Sha => format!("sha {}", self.short_sha()),
        }
    }
}

/// File-system pin store. The store does NOT cache — every read walks
/// the overlay directory afresh, so concurrent edits between processes
/// (rare, but possible) don't surface stale state.
pub struct PinStore {
    root: PathBuf,
}

impl PinStore {
    /// Open the user-level overlay store at `~/.config/pay/skills/overlay/`.
    pub fn open_default() -> Self {
        Self {
            root: PathBuf::from(shellexpand::tilde(OVERLAY_DIR).into_owned()),
        }
    }

    /// Open a store rooted at an arbitrary directory. Used by tests.
    pub fn open_at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Root overlay directory. Stable absolute path even before the
    /// directory exists on disk.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding a specific FQN's files (does not check existence).
    pub fn dir_for(&self, fqn: &str) -> Result<PathBuf> {
        validate_fqn(fqn)?;
        let mut dir = self.root.clone();
        for segment in fqn.split('/') {
            dir.push(segment);
        }
        Ok(dir)
    }

    /// Read every pin currently installed. Pins with a malformed manifest
    /// are skipped with a warning rather than failing the whole load.
    ///
    /// Best-effort sweep of any `.staging-*` / `.trash-*` directories left
    /// behind by a previously-crashed `upsert`. Without this, every read
    /// path would observe the cleanup obligation, but only `upsert` would
    /// discharge it.
    pub fn read_all(&self) -> Vec<(PinManifest, PathBuf)> {
        sweep_temp_dirs(&self.root);
        let mut out = Vec::new();
        self.walk(&self.root.clone(), &mut Vec::new(), &mut out);
        out
    }

    fn walk(&self, dir: &Path, segments: &mut Vec<String>, out: &mut Vec<(PinManifest, PathBuf)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let manifest_path = dir.join(PIN_MANIFEST_FILE);
        if manifest_path.is_file() {
            match fs::read_to_string(&manifest_path).and_then(|raw| {
                serde_json::from_str::<PinManifest>(&raw).map_err(std::io::Error::other)
            }) {
                Ok(m) => out.push((m, dir.to_path_buf())),
                Err(e) => {
                    tracing::warn!(?manifest_path, ?e, "skipping malformed pin");
                }
            }
            return;
        }
        for entry in entries.flatten() {
            // Skip transient directories left by a partially-completed
            // upsert that died between renames: `.staging-*` holds the
            // new pin while it's being assembled, `.trash-*` holds the
            // previous pin between the park rename and the publish
            // rename. Both contain real `.pin.json` files; without this
            // guard a crashed install could surface a stale (or
            // duplicate-FQN) pin until the next upsert sweeps them.
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".staging-") || name.starts_with(".trash-") {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                segments.push(name);
                self.walk(&path, segments, out);
                segments.pop();
            }
        }
    }

    /// Look up a single pin by FQN.
    pub fn get(&self, fqn: &str) -> Result<Option<PinManifest>> {
        let manifest_path = self.dir_for(fqn)?.join(PIN_MANIFEST_FILE);
        if !manifest_path.is_file() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|e| Error::Config(format!("read {}: {e}", manifest_path.display())))?;
        let manifest: PinManifest = serde_json::from_str(&raw)
            .map_err(|e| Error::Config(format!("parse {}: {e}", manifest_path.display())))?;
        Ok(Some(manifest))
    }

    /// Install a pin atomically: write all files to a staging directory,
    /// then rename it into place over any existing pin at the same FQN.
    /// The previous pin's files survive a partial-write failure.
    ///
    /// `files` is `(relative_path, content_bytes)`. Each file's sha256 is
    /// computed and included in the persisted manifest. Total size is
    /// checked against [`MAX_PIN_BYTES`].
    pub fn upsert(&self, manifest: &mut PinManifest, files: &[(String, Vec<u8>)]) -> Result<()> {
        validate_fqn(&manifest.fqn)?;

        let total: u64 = files.iter().map(|(_, b)| b.len() as u64).sum();
        if total > MAX_PIN_BYTES {
            return Err(Error::Config(format!(
                "pin {} would write {total} bytes (cap: {MAX_PIN_BYTES})",
                manifest.fqn
            )));
        }

        // Stage in a sibling temp dir under the overlay root so the final
        // rename is on the same filesystem (and therefore atomic).
        fs::create_dir_all(&self.root)
            .map_err(|e| Error::Config(format!("mkdir {}: {e}", self.root.display())))?;
        let staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&self.root)
            .map_err(|e| Error::Config(format!("staging dir: {e}")))?;

        let mut file_entries: Vec<PinFile> = Vec::with_capacity(files.len());
        for (rel, bytes) in files {
            validate_relative_path(rel)?;
            let abs = staging.path().join(rel);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| Error::Config(format!("mkdir {}: {e}", parent.display())))?;
            }
            let mut f = fs::File::create(&abs)
                .map_err(|e| Error::Config(format!("create {}: {e}", abs.display())))?;
            f.write_all(bytes)
                .map_err(|e| Error::Config(format!("write {}: {e}", abs.display())))?;
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            file_entries.push(PinFile {
                path: rel.clone(),
                sha256: hex(&hasher.finalize()),
                size: bytes.len() as u64,
            });
        }
        manifest.files = file_entries;

        let manifest_bytes = serde_json::to_vec_pretty(&*manifest)
            .map_err(|e| Error::Config(format!("serialize pin manifest: {e}")))?;
        let manifest_path = staging.path().join(PIN_MANIFEST_FILE);
        fs::write(&manifest_path, &manifest_bytes)
            .map_err(|e| Error::Config(format!("write {}: {e}", manifest_path.display())))?;

        let final_dir = self.dir_for(&manifest.fqn)?;
        if let Some(parent) = final_dir.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        // If a previous pin lives here, swap-and-delete it.
        if final_dir.exists() {
            // tempdir-in-overlay so the rename is same-fs
            let trash = tempfile::Builder::new()
                .prefix(".trash-")
                .tempdir_in(&self.root)
                .map_err(|e| Error::Config(format!("trash dir: {e}")))?;
            let trash_target = trash.path().join("old");
            fs::rename(&final_dir, &trash_target)
                .map_err(|e| Error::Config(format!("park existing: {e}")))?;
            let staged = staging.keep();
            if let Err(e) = fs::rename(&staged, &final_dir) {
                // Roll back: put the old pin back, drop the staging.
                let _ = fs::rename(&trash_target, &final_dir);
                let _ = fs::remove_dir_all(&staged);
                return Err(Error::Config(format!("publish pin: {e}")));
            }
            // tempdir's TempDir Drop nukes the trash; explicit close()
            // also fine. Either way the old pin is gone.
            drop(trash);
        } else {
            let staged = staging.keep();
            fs::rename(&staged, &final_dir)
                .map_err(|e| Error::Config(format!("publish pin: {e}")))?;
        }
        // Sweep any orphan staging/trash dirs that piled up from earlier
        // crashes. Cheap, idempotent.
        sweep_temp_dirs(&self.root);
        Ok(())
    }

    /// Remove a single pin. Returns true if it existed and was removed.
    /// Also prunes now-empty parent directories up to the overlay root.
    pub fn remove(&self, fqn: &str) -> Result<bool> {
        let dir = self.dir_for(fqn)?;
        if !dir.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&dir)
            .map_err(|e| Error::Config(format!("remove {}: {e}", dir.display())))?;
        // Walk up and drop empty parents (but never past root).
        let mut parent = dir.parent();
        while let Some(p) = parent {
            if p == self.root {
                break;
            }
            let is_empty = match fs::read_dir(p) {
                Ok(mut it) => it.next().is_none(),
                Err(_) => false,
            };
            if is_empty {
                let _ = fs::remove_dir(p);
                parent = p.parent();
            } else {
                break;
            }
        }
        Ok(true)
    }
}

/// FQN syntax: one or more `[a-z0-9._-]+` segments separated by `/`,
/// no leading/trailing slash, no path traversal.
fn validate_fqn(fqn: &str) -> Result<()> {
    if fqn.is_empty() {
        return Err(Error::Config("fqn cannot be empty".into()));
    }
    if fqn.starts_with('/') || fqn.ends_with('/') {
        return Err(Error::Config(format!("invalid fqn: {fqn}")));
    }
    for segment in fqn.split('/') {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(Error::Config(format!("invalid fqn segment {segment:?}")));
        }
    }
    Ok(())
}

/// Relative file path under a pin's directory. Same rules as FQN
/// segments + must not be absolute.
fn validate_relative_path(rel: &str) -> Result<()> {
    if rel.is_empty() || rel.starts_with('/') {
        return Err(Error::Config(format!("invalid relative path: {rel}")));
    }
    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(Error::Config(format!("invalid path segment {segment:?}")));
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn sweep_temp_dirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(".staging-") || name.starts_with(".trash-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Public utility: pretty BTreeMap helper for tests / debug dumps.
pub fn pins_by_fqn(pins: Vec<(PinManifest, PathBuf)>) -> BTreeMap<String, (PinManifest, PathBuf)> {
    pins.into_iter()
        .map(|(m, p)| (m.fqn.clone(), (m, p)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn manifest(fqn: &str) -> PinManifest {
        PinManifest {
            fqn: fqn.to_string(),
            source_repo: "solana-foundation/pay-skills".to_string(),
            head_repo: "fork/pay-skills".to_string(),
            anchor: PinAnchor::Pr {
                pr: 137,
                head_ref: "feat/x".to_string(),
            },
            sha: "abcdef1234567890".to_string(),
            installed_at: "2026-06-12T00:00:00Z".to_string(),
            merged: false,
            files: vec![],
        }
    }

    #[test]
    fn roundtrip_pin_manifest() {
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        let mut m = manifest("venice/ai");
        store
            .upsert(
                &mut m,
                &[("PAY.md".to_string(), b"---\nname: ai\n---\nhi".to_vec())],
            )
            .unwrap();

        let read = store.get("venice/ai").unwrap().unwrap();
        assert_eq!(read.fqn, "venice/ai");
        assert_eq!(read.files.len(), 1);
        assert_eq!(read.files[0].path, "PAY.md");
        assert_eq!(read.files[0].size, 19);
    }

    #[test]
    fn read_all_ignores_orphan_trash_and_staging_dirs() {
        // Simulate a crash mid-`upsert`: the old pin has been parked
        // under `.trash-<rand>/old/`, and a fresh `.staging-<rand>/`
        // is half-built. Both have valid `.pin.json` payloads, so a
        // naive `walk()` would surface them. Verify `read_all()`
        // skips them and the on-disk pin is the only one returned.
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        let mut m = manifest("venice/ai");
        store
            .upsert(&mut m, &[("PAY.md".to_string(), b"live".to_vec())])
            .unwrap();

        let trash_root = dir.path().join(".trash-deadbeef");
        let trash_old = trash_root.join("old");
        std::fs::create_dir_all(&trash_old).unwrap();
        std::fs::write(
            trash_old.join(".pin.json"),
            serde_json::to_vec(&manifest("venice/ai")).unwrap(),
        )
        .unwrap();

        let staging = dir.path().join(".staging-cafebabe");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(
            staging.join(".pin.json"),
            serde_json::to_vec(&manifest("venice/ai")).unwrap(),
        )
        .unwrap();

        let pins = store.read_all();
        assert_eq!(pins.len(), 1, "trash and staging dirs must not surface");
        assert_eq!(pins[0].0.fqn, "venice/ai");
        // read_all also sweeps the orphans so they don't accumulate.
        assert!(!trash_root.exists(), "trash dir should be swept");
        assert!(!staging.exists(), "staging dir should be swept");
    }

    #[test]
    fn upsert_replaces_existing_pin_atomically() {
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        let mut m1 = manifest("venice/ai");
        store
            .upsert(&mut m1, &[("PAY.md".to_string(), b"v1".to_vec())])
            .unwrap();
        let mut m2 = manifest("venice/ai");
        m2.sha = "00000000".to_string();
        store
            .upsert(&mut m2, &[("PAY.md".to_string(), b"v2-bigger".to_vec())])
            .unwrap();
        let read = store.get("venice/ai").unwrap().unwrap();
        assert_eq!(read.sha, "00000000");
        assert_eq!(read.files[0].size, 9);
        // Only one pin exists for that fqn.
        let all = store.read_all();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn read_all_walks_multiple_pins() {
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        for fqn in ["venice/ai", "acme/billing", "deep/nested/svc"] {
            let mut m = manifest(fqn);
            store
                .upsert(&mut m, &[("PAY.md".to_string(), b"x".to_vec())])
                .unwrap();
        }
        let pins = pins_by_fqn(store.read_all());
        assert!(pins.contains_key("venice/ai"));
        assert!(pins.contains_key("acme/billing"));
        assert!(pins.contains_key("deep/nested/svc"));
    }

    #[test]
    fn remove_prunes_empty_parents() {
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        let mut m = manifest("acme/billing");
        store
            .upsert(&mut m, &[("PAY.md".to_string(), b"x".to_vec())])
            .unwrap();
        assert!(store.remove("acme/billing").unwrap());
        assert!(
            !dir.path().join("acme").exists(),
            "empty parent should be pruned"
        );
        assert!(!store.remove("acme/billing").unwrap());
    }

    #[test]
    fn invalid_fqn_rejected() {
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        let mut m = manifest("../escape");
        assert!(
            store
                .upsert(&mut m, &[("PAY.md".to_string(), b"x".to_vec())])
                .is_err()
        );
        let mut m = manifest("a//b");
        assert!(
            store
                .upsert(&mut m, &[("PAY.md".to_string(), b"x".to_vec())])
                .is_err()
        );
    }

    #[test]
    fn relative_path_traversal_rejected() {
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        let mut m = manifest("venice/ai");
        assert!(
            store
                .upsert(&mut m, &[("../escape".to_string(), b"x".to_vec())])
                .is_err()
        );
        assert!(
            store
                .upsert(&mut m, &[("/abs".to_string(), b"x".to_vec())])
                .is_err()
        );
    }

    #[test]
    fn size_cap_enforced() {
        let dir = tempdir().unwrap();
        let store = PinStore::open_at(dir.path());
        let mut m = manifest("venice/ai");
        let huge = vec![0u8; (MAX_PIN_BYTES + 1) as usize];
        assert!(store.upsert(&mut m, &[("big".to_string(), huge)]).is_err());
    }

    #[test]
    fn anchor_label_formatting() {
        let m = manifest("venice/ai");
        assert_eq!(m.anchor_label(), "PR 137");
        assert_eq!(m.short_sha(), "abcdef1");
    }
}
