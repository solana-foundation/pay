//! Minimal GitHub REST client for pin installs.
//!
//! Three operations:
//!   - resolve a PR → head repo / head sha / head branch / merged status
//!   - list a directory at a specific tree (returns blob shas + sizes)
//!   - fetch a blob by sha
//!
//! Anonymous by default; respects `GITHUB_TOKEN` when set. Rate limits are
//! 60/h anon, 5000/h authed.
//!
//! Defensive against malicious upstream:
//!   - rejects directories above [`MAX_TREE_ENTRIES`]
//!   - rejects entries whose path tries to escape the requested subtree
//!     (`..`, absolute paths, symlinks)
//!   - blob fetches enforce a per-file size cap separate from the
//!     pin-total cap in [`crate::skills::pin::MAX_PIN_BYTES`]

use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

use crate::{Error, Result};

fn url_encode(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

const API_BASE: &str = "https://api.github.com";
const USER_AGENT: &str = "pay-cli";

/// Max files a single pin can install. The pay-skills convention is a
/// handful per provider — bumping into this limit usually means the
/// caller asked for the wrong path.
pub const MAX_TREE_ENTRIES: usize = 256;

/// Max bytes for a single blob. Pin total is still bounded by
/// [`crate::skills::pin::MAX_PIN_BYTES`].
pub const MAX_BLOB_BYTES: u64 = 2 * 1024 * 1024;

/// Resolved description of a pull request.
#[derive(Debug, Clone)]
pub struct PrInfo {
    /// `owner/repo` of the head repository (the fork, if any).
    pub head_repo: String,
    /// Commit SHA at the head of the PR.
    pub head_sha: String,
    /// Head branch name on `head_repo`.
    pub head_ref: String,
    /// True once the PR is merged.
    pub merged: bool,
}

/// One entry inside a tree listing.
#[derive(Debug, Clone)]
pub struct TreeEntry {
    /// Path relative to the listed directory (forward slashes).
    pub path: String,
    /// Blob SHA (used to fetch content).
    pub sha: String,
    /// File size in bytes.
    pub size: u64,
}

/// Build a `reqwest` blocking client with our default headers + 30s
/// timeout. Used inside the blocking wrappers; for the async path we
/// rebuild a tokio-aware client instead.
fn blocking_client() -> Result<reqwest::blocking::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static(USER_AGENT),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
    );
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(reqwest::header::AUTHORIZATION, val);
    }
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .default_headers(headers)
        .build()
        .map_err(|e| Error::Config(format!("build http client: {e}")))
}

/// Look up a pull request by number on `owner/repo`. Returns the head
/// repo (could differ from `repo` for cross-fork PRs), head SHA, head
/// ref name, and merged status.
pub fn resolve_pr(repo: &str, pr: u32) -> Result<PrInfo> {
    validate_repo(repo)?;
    let client = blocking_client()?;
    let url = format!("{API_BASE}/repos/{repo}/pulls/{pr}");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| Error::Config(format!("github GET {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(github_error(status, &resp.text().unwrap_or_default(), &url));
    }
    let body: PrResponse = resp
        .json()
        .map_err(|e| Error::Config(format!("parse pr response: {e}")))?;
    // GitHub returns `head.repo: null` once the PR's source fork is
    // deleted. The commit is still reachable through the base repo's
    // refs/pull/<N>/head, so fall back rather than failing on parse.
    let head_repo = body
        .head
        .repo
        .map(|r| r.full_name)
        .unwrap_or_else(|| repo.to_string());
    Ok(PrInfo {
        head_repo,
        head_sha: body.head.sha,
        head_ref: body.head.r#ref,
        merged: body.merged,
    })
}

/// Resolve a branch ref to its current head SHA on `owner/repo`.
pub fn resolve_branch(repo: &str, ref_name: &str) -> Result<String> {
    validate_repo(repo)?;
    let client = blocking_client()?;
    let url = format!("{API_BASE}/repos/{repo}/branches/{}", url_encode(ref_name));
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| Error::Config(format!("github GET {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(github_error(status, &resp.text().unwrap_or_default(), &url));
    }
    let body: BranchResponse = resp
        .json()
        .map_err(|e| Error::Config(format!("parse branch response: {e}")))?;
    Ok(body.commit.sha)
}

/// List every file under `path` at the given tree-ish (`ref` can be a
/// SHA, branch name, or tag). Returns a flat list (recursive walk).
pub fn list_directory(repo: &str, tree_ish: &str, path: &str) -> Result<Vec<TreeEntry>> {
    validate_repo(repo)?;
    validate_tree_path(path)?;

    let client = blocking_client()?;
    let url = format!(
        "{API_BASE}/repos/{repo}/git/trees/{}?recursive=1",
        url_encode(tree_ish)
    );
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| Error::Config(format!("github GET {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(github_error(status, &resp.text().unwrap_or_default(), &url));
    }
    let body: TreeResponse = resp
        .json()
        .map_err(|e| Error::Config(format!("parse tree response: {e}")))?;
    if body.truncated {
        return Err(Error::Config(format!(
            "tree listing for {repo}@{tree_ish} was truncated; pin path too broad?"
        )));
    }

    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("{}/", path.trim_end_matches('/'))
    };
    let mut out = Vec::new();
    for entry in body.tree {
        if entry.r#type != "blob" {
            continue;
        }
        // Filter to entries under the requested subtree.
        let rel = if prefix.is_empty() {
            Some(entry.path.as_str())
        } else {
            entry.path.strip_prefix(&prefix)
        };
        let Some(rel) = rel else { continue };
        if rel.is_empty() {
            continue;
        }
        validate_tree_path(rel)?;
        out.push(TreeEntry {
            path: rel.to_string(),
            sha: entry.sha,
            size: entry.size.unwrap_or(0),
        });
        if out.len() > MAX_TREE_ENTRIES {
            return Err(Error::Config(format!(
                "{repo}@{tree_ish}:{path} has more than {MAX_TREE_ENTRIES} files"
            )));
        }
    }
    Ok(out)
}

/// Fetch a single blob by SHA. Returns the raw content bytes.
pub fn fetch_blob(repo: &str, blob_sha: &str) -> Result<Vec<u8>> {
    validate_repo(repo)?;
    validate_blob_sha(blob_sha)?;

    let client = blocking_client()?;
    let url = format!("{API_BASE}/repos/{repo}/git/blobs/{blob_sha}");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| Error::Config(format!("github GET {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(github_error(status, &resp.text().unwrap_or_default(), &url));
    }
    let body: BlobResponse = resp
        .json()
        .map_err(|e| Error::Config(format!("parse blob response: {e}")))?;
    if body.size > MAX_BLOB_BYTES {
        return Err(Error::Config(format!(
            "blob {blob_sha} is {} bytes (cap: {MAX_BLOB_BYTES})",
            body.size
        )));
    }
    match body.encoding.as_str() {
        "base64" => {
            use base64::Engine as _;
            let cleaned: String = body
                .content
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            base64::engine::general_purpose::STANDARD
                .decode(&cleaned)
                .map_err(|e| Error::Config(format!("decode blob: {e}")))
        }
        other => Err(Error::Config(format!("unsupported blob encoding: {other}"))),
    }
}

fn validate_repo(repo: &str) -> Result<()> {
    if repo.is_empty() {
        return Err(Error::Config("repo cannot be empty".into()));
    }
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        return Err(Error::Config(format!(
            "repo must be 'owner/name', got {repo}"
        )));
    }
    for p in parts {
        if !p
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(Error::Config(format!("invalid repo segment {p:?}")));
        }
    }
    Ok(())
}

fn validate_tree_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    if path.starts_with('/') {
        return Err(Error::Config(format!(
            "tree path must be relative, got {path}"
        )));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(Error::Config(format!(
                "tree path has empty or traversal segment: {path}"
            )));
        }
    }
    Ok(())
}

fn validate_blob_sha(sha: &str) -> Result<()> {
    if sha.len() < 7 || sha.len() > 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Config(format!("invalid blob sha: {sha}")));
    }
    Ok(())
}

fn github_error(status: reqwest::StatusCode, body: &str, url: &str) -> Error {
    // GitHub returns JSON error bodies — surface the `message` if we can.
    let msg = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| body.chars().take(200).collect());
    Error::Config(format!("github {status} on {url}: {msg}"))
}

// ── Wire shapes (subset of GitHub's responses) ─────────────────────────────

#[derive(Deserialize)]
struct PrResponse {
    head: PrHead,
    merged: bool,
}

#[derive(Deserialize)]
struct PrHead {
    sha: String,
    r#ref: String,
    /// `head.repo` is `null` when the PR's source fork has been deleted.
    /// The base repo still serves the head SHA via `refs/pull/<N>/head`,
    /// so we fall back to the base repo in `resolve_pr`.
    #[serde(default)]
    repo: Option<PrHeadRepo>,
}

#[derive(Deserialize)]
struct PrHeadRepo {
    full_name: String,
}

#[derive(Deserialize)]
struct BranchResponse {
    commit: BranchCommit,
}

#[derive(Deserialize)]
struct BranchCommit {
    sha: String,
}

#[derive(Deserialize)]
struct TreeResponse {
    tree: Vec<TreeBlob>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeBlob {
    path: String,
    sha: String,
    r#type: String,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Deserialize)]
struct BlobResponse {
    encoding: String,
    content: String,
    size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_validation() {
        assert!(validate_repo("solana-foundation/pay-skills").is_ok());
        assert!(validate_repo("a/b").is_ok());
        assert!(validate_repo("solana-foundation").is_err());
        assert!(validate_repo("a/b/c").is_err());
        assert!(validate_repo("/b").is_err());
        assert!(validate_repo("a/").is_err());
        assert!(validate_repo("a b/c").is_err());
    }

    #[test]
    fn tree_path_validation() {
        assert!(validate_tree_path("").is_ok());
        assert!(validate_tree_path("providers/venice/ai").is_ok());
        assert!(validate_tree_path("/abs").is_err());
        assert!(validate_tree_path("../escape").is_err());
        assert!(validate_tree_path("ok/../bad").is_err());
        // Reject empty segments — otherwise a path like "a//b" or "a/b/"
        // would slip through here but trip the stricter `validate_relative_path`
        // later, producing a confusing error deep in PinStore::upsert.
        assert!(validate_tree_path("a//b").is_err());
        assert!(validate_tree_path("a/b/").is_err());
    }

    #[test]
    fn blob_sha_validation() {
        assert!(validate_blob_sha("abc1234").is_ok());
        assert!(validate_blob_sha("a".repeat(40).as_str()).is_ok());
        assert!(validate_blob_sha("zzz1234").is_err());
        assert!(validate_blob_sha("short").is_err());
    }

    #[test]
    fn pr_head_deserializes_with_null_repo() {
        // GitHub returns `head.repo: null` after the source fork is
        // deleted. The deserializer must accept it so `resolve_pr`
        // can fall back to the base repo for blob fetches.
        let raw = r#"{
            "merged": false,
            "head": { "sha": "abc1234", "ref": "feat/x", "repo": null }
        }"#;
        let pr: PrResponse = serde_json::from_str(raw).expect("null head.repo must parse");
        assert!(pr.head.repo.is_none());
        assert_eq!(pr.head.sha, "abc1234");

        // …and when `head.repo` is missing entirely, same fallback.
        let raw = r#"{ "merged": true, "head": { "sha": "def5678", "ref": "feat/y" } }"#;
        let pr: PrResponse = serde_json::from_str(raw).expect("missing head.repo must parse");
        assert!(pr.head.repo.is_none());

        // Populated repo still parses through.
        let raw = r#"{
            "merged": false,
            "head": { "sha": "1", "ref": "x", "repo": { "full_name": "fork/repo" } }
        }"#;
        let pr: PrResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(pr.head.repo.unwrap().full_name, "fork/repo");
    }
}
