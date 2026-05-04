//! Solana-compatibility verdict: classify probe results into ok / non-Solana /
//! free / indeterminate / error and aggregate them per provider. Used by
//! `pay catalog build` to surface a verdict alongside its build/dist output.
//!
//! - **Warning** for every gated endpoint that doesn't accept Solana
//!   stablecoin payment (e.g. Base-only).
//! - **Error** when a provider has zero gated endpoints that accept Solana,
//!   i.e. nothing in the diff actually works through pay's wallet.
//! - Indeterminate statuses (`siwx_required`, `auth_required`,
//!   `unprobeable_needs_body`, `not_found`, …) neither warn nor error — they
//!   pass through silently because the probe couldn't classify them.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use owo_colors::OwoColorize;
use serde::Serialize;

use pay_core::skills::probe::{EndpointProbeResult, ProbeReport, ProviderProbeResult};

/// Categorize a single endpoint result against the Solana-compat gate.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndpointVerdict {
    /// Gated and accepts Solana stablecoins. Counts toward "at least one ok".
    Ok,
    /// Gated but only accepts non-Solana chains (e.g. Base USDC). Surfaces a
    /// warning by default; an error under `--strict`.
    NotSolana,
    /// Free / not gated.
    Free,
    /// Indeterminate — auth, siwx, body required, 404, etc. Does not count
    /// either way. Surfaced as info only.
    Indeterminate,
    /// Connection failure.
    Error,
}

/// Result of validating a single endpoint within a provider.
#[derive(Debug, Clone, Serialize)]
pub struct EndpointVerdictRow {
    pub method: String,
    pub path: String,
    pub probe_status: String,
    pub verdict: EndpointVerdict,
    /// Short, human-readable reason for the verdict.
    pub note: String,
}

/// Per-provider validation outcome.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderVerdict {
    pub fqn: String,
    pub file: String,
    pub endpoints: Vec<EndpointVerdictRow>,
    /// Total number of `ok` endpoints (Solana-compatible).
    pub ok_count: usize,
    /// Total number of `not_solana` endpoints.
    pub non_solana_count: usize,
    /// Whether the provider blocks the PR (zero ok endpoints, or
    /// `--strict` and any non-Solana endpoint).
    pub block: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub providers: Vec<ProviderVerdict>,
    pub strict: bool,
}

impl ValidationReport {
    pub fn has_errors(&self) -> bool {
        self.providers.iter().any(|p| p.block)
    }
}

/// Apply the validation rules to a probe report. `strict` upgrades every
/// `NotSolana` endpoint to a blocking error.
pub fn validate_report(report: &ProbeReport, strict: bool) -> ValidationReport {
    let providers = report
        .providers
        .iter()
        .map(|p| verdict_for_provider(p, strict))
        .collect();
    ValidationReport { providers, strict }
}

fn verdict_for_provider(provider: &ProviderProbeResult, strict: bool) -> ProviderVerdict {
    let endpoints: Vec<EndpointVerdictRow> = provider
        .endpoints
        .iter()
        .map(verdict_for_endpoint)
        .collect();

    let ok_count = endpoints
        .iter()
        .filter(|e| e.verdict == EndpointVerdict::Ok)
        .count();
    let non_solana_count = endpoints
        .iter()
        .filter(|e| e.verdict == EndpointVerdict::NotSolana)
        .count();

    // Total of Solana-relevant endpoints (gated, classifiable). If zero such
    // endpoints exist, we can't make a verdict — pass through.
    let total_classified = ok_count + non_solana_count;
    let block = if total_classified == 0 {
        false
    } else if strict {
        non_solana_count > 0
    } else {
        ok_count == 0
    };

    ProviderVerdict {
        fqn: provider.fqn.clone(),
        file: provider_md_path(&provider.fqn),
        endpoints,
        ok_count,
        non_solana_count,
        block,
    }
}

fn verdict_for_endpoint(ep: &EndpointProbeResult) -> EndpointVerdictRow {
    let (verdict, note) = match ep.probe_status.as_str() {
        "ok" => (
            EndpointVerdict::Ok,
            format!("paid via {}", ep.paid.protocols.join(",")),
        ),
        "wrong_chain" => (
            EndpointVerdict::NotSolana,
            "gated but no Solana scheme advertised".into(),
        ),
        "wrong_currency" => (
            EndpointVerdict::NotSolana,
            "gated on Solana but with non-USD-stable currency".into(),
        ),
        "free" => (EndpointVerdict::Free, "free / not gated".into()),
        "siwx_required" => (
            EndpointVerdict::Indeterminate,
            "SIWX-only — payment behind sign-in, can't verify".into(),
        ),
        "auth_required" => (
            EndpointVerdict::Indeterminate,
            "auth required — payment behind credentials, can't verify".into(),
        ),
        "unprobeable_needs_body" => (
            EndpointVerdict::Indeterminate,
            "server rejected empty/dummy body before paywall".into(),
        ),
        "not_found" => (
            EndpointVerdict::Indeterminate,
            "404 — endpoint may have been moved or removed".into(),
        ),
        "method_not_allowed" => (
            EndpointVerdict::Indeterminate,
            "405 — method/path mismatch in spec".into(),
        ),
        "error" => (
            EndpointVerdict::Error,
            "probe failed (network/timeout)".into(),
        ),
        other => (
            EndpointVerdict::Indeterminate,
            format!("unclassified probe status `{other}`"),
        ),
    };

    EndpointVerdictRow {
        method: ep.method.clone(),
        path: ep.path.clone(),
        probe_status: ep.probe_status.clone(),
        verdict,
        note,
    }
}

fn provider_md_path(fqn: &str) -> String {
    // FQN matches the relative path under providers/ to the provider's
    // directory; PAY.md lives inside that directory (e.g.
    // `merit-systems/stabledomains/domains` →
    // `providers/merit-systems/stabledomains/domains/PAY.md`).
    format!("providers/{fqn}/PAY.md")
}

// ── git-diff plumbing ───────────────────────────────────────────────────────

pub fn git_changed_provider_files(
    repo_root: &Path,
    base_ref: &str,
) -> pay_core::Result<Vec<PathBuf>> {
    let output = ProcessCommand::new("git")
        .args(["diff", "--name-only", "--diff-filter=ACMR"])
        .arg(format!("{base_ref}...HEAD"))
        .current_dir(repo_root)
        .output()
        .map_err(|e| pay_core::Error::Config(format!("git diff failed: {e}")))?;

    if !output.status.success() {
        return Err(pay_core::Error::Config(format!(
            "git diff exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Resolve every changed file under `providers/` to the PAY.md of its
    // containing provider. This catches sidecar files (e.g. `openapi.json`,
    // schemas) that don't end in `/PAY.md` but still belong to a provider —
    // a change there must rebuild the provider's dist entry. Files that
    // can't be resolved (e.g. a deleted provider's whole directory) are
    // dropped silently.
    let mut seen = std::collections::BTreeSet::new();
    for line in stdout.lines() {
        if !line.starts_with("providers/") {
            continue;
        }
        if let Some(pay_md) = resolve_to_pay_md(repo_root, Path::new(line)) {
            seen.insert(pay_md);
        }
    }
    Ok(seen.into_iter().collect())
}

/// Walk up from a changed file to the nearest ancestor directory that
/// contains a `PAY.md`. Returns the relative path to that `PAY.md` (under
/// the repo root) or `None` if no such ancestor exists. The walk stops at
/// `providers/` so we never escape the registry.
fn resolve_to_pay_md(repo_root: &Path, changed: &Path) -> Option<PathBuf> {
    // If the changed file IS a PAY.md, use it directly.
    if changed.file_name() == Some(std::ffi::OsStr::new("PAY.md")) {
        return Some(changed.to_path_buf());
    }
    let mut cursor = changed.parent()?;
    loop {
        // Stop once we've walked above `providers/`.
        if cursor == Path::new("providers") || cursor.as_os_str().is_empty() {
            return None;
        }
        let candidate_rel = cursor.join("PAY.md");
        if repo_root.join(&candidate_rel).is_file() {
            return Some(candidate_rel);
        }
        cursor = cursor.parent()?;
    }
}

// ── renderers ──────────────────────────────────────────────────────────────

pub fn render_verdict_table(report: &ValidationReport) {
    let mut block_count = 0;
    let mut warn_count = 0;
    for provider in &report.providers {
        let header = if provider.block {
            format!(
                "{}  {} ({}/{})",
                "BLOCK".red().bold(),
                provider.fqn.bold(),
                provider.ok_count,
                provider.ok_count + provider.non_solana_count
            )
        } else if provider.non_solana_count > 0 {
            format!(
                "{}   {} ({}/{})",
                "WARN".yellow().bold(),
                provider.fqn.bold(),
                provider.ok_count,
                provider.ok_count + provider.non_solana_count
            )
        } else {
            format!(
                "{}   {} ({}/{})",
                "PASS".green().bold(),
                provider.fqn.bold(),
                provider.ok_count,
                provider.ok_count + provider.non_solana_count
            )
        };
        eprintln!("{header}");
        if provider.block {
            block_count += 1;
        }
        for ep in &provider.endpoints {
            let icon = match ep.verdict {
                EndpointVerdict::Ok => "OK".green().to_string(),
                EndpointVerdict::NotSolana => {
                    warn_count += 1;
                    "WARN".yellow().to_string()
                }
                EndpointVerdict::Free => "FREE".dimmed().to_string(),
                EndpointVerdict::Indeterminate => "?".dimmed().to_string(),
                EndpointVerdict::Error => "ERR".red().to_string(),
            };
            eprintln!(
                "  {icon}  {} {}  {} ({})",
                ep.method.dimmed(),
                ep.path,
                ep.note.dimmed(),
                ep.probe_status.dimmed()
            );
        }
    }
    let _ = (block_count, warn_count);
}

pub fn render_verdict_json(report: &ValidationReport) -> pay_core::Result<()> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
    println!("{json}");
    Ok(())
}

pub fn render_verdict_github(report: &ValidationReport) {
    // GitHub Actions workflow-command annotations:
    //   ::error file=...,title=...::message
    //   ::warning file=...,title=...::message
    // Multi-line messages are escaped per Actions spec (% → %25, \r → %0D, \n → %0A).
    for provider in &report.providers {
        for ep in &provider.endpoints {
            match ep.verdict {
                EndpointVerdict::NotSolana => {
                    let msg = format!("{} {} — {}", ep.method, ep.path, ep.note);
                    let annotation = if report.strict { "error" } else { "warning" };
                    println!(
                        "::{annotation} file={file},title={title}::{msg}",
                        file = provider.file,
                        title = encode_actions(&format!("{}: non-Solana endpoint", provider.fqn)),
                        msg = encode_actions(&msg),
                    );
                }
                EndpointVerdict::Error => {
                    println!(
                        "::warning file={file},title={title}::{msg}",
                        file = provider.file,
                        title = encode_actions(&format!("{}: probe error", provider.fqn)),
                        msg = encode_actions(&format!("{} {} — {}", ep.method, ep.path, ep.note)),
                    );
                }
                _ => {}
            }
        }
        if provider.block {
            let msg = format!(
                "{}: 0 of {} classifiable endpoints accept Solana stablecoins. \
                 At least one Solana-compatible endpoint is required.",
                provider.fqn,
                provider.ok_count + provider.non_solana_count,
            );
            println!(
                "::error file={file},title={title}::{msg}",
                file = provider.file,
                title =
                    encode_actions(&format!("{}: no Solana-compatible endpoints", provider.fqn)),
                msg = encode_actions(&msg),
            );
        }
    }

    let total_blocks = report.providers.iter().filter(|p| p.block).count();
    let total_warns: usize = report.providers.iter().map(|p| p.non_solana_count).sum();
    let summary = if total_blocks > 0 {
        format!(
            "{} provider(s) blocked, {} non-Solana endpoint(s)",
            total_blocks, total_warns,
        )
    } else if total_warns > 0 {
        format!("{} non-Solana endpoint(s) flagged", total_warns)
    } else {
        "all changed providers Solana-compatible".to_string()
    };
    println!(
        "::notice title=pay-skills validation::{}",
        encode_actions(&summary)
    );
}

/// Escape a string for inclusion in a GitHub Actions `::cmd::message`.
fn encode_actions(msg: &str) -> String {
    msg.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pay_core::skills::probe::{PaidEndpoint, ProbeStatus};

    fn ep_result(probe_status: &str, paid_protocols: Vec<&str>) -> EndpointProbeResult {
        EndpointProbeResult {
            method: "POST".into(),
            path: "v1/foo".into(),
            url: "https://api.example.com/v1/foo".into(),
            status: ProbeStatus::Free,
            paid: PaidEndpoint {
                protocols: paid_protocols.into_iter().map(String::from).collect(),
                ..Default::default()
            },
            probe_status: probe_status.into(),
            http_status: 402,
            duration_ms: 100,
        }
    }

    fn provider_result(fqn: &str, eps: Vec<EndpointProbeResult>) -> ProviderProbeResult {
        ProviderProbeResult {
            fqn: fqn.into(),
            service_url: "https://api.example.com".into(),
            endpoints: eps,
            pass: true,
        }
    }

    #[test]
    fn block_when_zero_ok_and_some_non_solana() {
        let report = ProbeReport {
            providers: vec![provider_result(
                "foo/bar",
                vec![ep_result("wrong_chain", vec![])],
            )],
            total_endpoints: 1,
            passed: 0,
            failed: 1,
        };
        let v = validate_report(&report, false);
        assert!(v.providers[0].block);
        assert_eq!(v.providers[0].ok_count, 0);
        assert_eq!(v.providers[0].non_solana_count, 1);
    }

    #[test]
    fn pass_when_at_least_one_ok() {
        let report = ProbeReport {
            providers: vec![provider_result(
                "foo/bar",
                vec![
                    ep_result("ok", vec!["x402"]),
                    ep_result("wrong_chain", vec![]),
                ],
            )],
            total_endpoints: 2,
            passed: 1,
            failed: 1,
        };
        let v = validate_report(&report, false);
        assert!(!v.providers[0].block);
        assert_eq!(v.providers[0].ok_count, 1);
        assert_eq!(v.providers[0].non_solana_count, 1);
    }

    #[test]
    fn pass_when_only_indeterminate_endpoints() {
        // siwx/auth/needs-body don't count either way — provider is not blocked.
        let report = ProbeReport {
            providers: vec![provider_result(
                "foo/bar",
                vec![
                    ep_result("siwx_required", vec![]),
                    ep_result("auth_required", vec![]),
                ],
            )],
            total_endpoints: 2,
            passed: 0,
            failed: 0,
        };
        let v = validate_report(&report, false);
        assert!(!v.providers[0].block);
        assert_eq!(v.providers[0].ok_count, 0);
        assert_eq!(v.providers[0].non_solana_count, 0);
    }

    #[test]
    fn strict_blocks_on_any_non_solana() {
        let report = ProbeReport {
            providers: vec![provider_result(
                "foo/bar",
                vec![
                    ep_result("ok", vec!["x402"]),
                    ep_result("wrong_chain", vec![]),
                ],
            )],
            total_endpoints: 2,
            passed: 1,
            failed: 1,
        };
        let v = validate_report(&report, true);
        assert!(v.providers[0].block);
    }

    #[test]
    fn provider_md_path_appends_pay_md() {
        assert_eq!(
            provider_md_path("merit-systems/stabledomains/domains"),
            "providers/merit-systems/stabledomains/domains/PAY.md"
        );
    }

    #[test]
    fn encode_actions_escapes_percent_and_newlines() {
        assert_eq!(encode_actions("a%b\nc\rd"), "a%25b%0Ac%0Dd");
    }

    #[test]
    fn resolve_to_pay_md_returns_self_for_pay_md() {
        let tmp = tempfile::tempdir().unwrap();
        let provider_dir = tmp.path().join("providers/foo/bar");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::write(provider_dir.join("PAY.md"), "").unwrap();

        let resolved =
            resolve_to_pay_md(tmp.path(), Path::new("providers/foo/bar/PAY.md")).unwrap();
        assert_eq!(resolved, PathBuf::from("providers/foo/bar/PAY.md"));
    }

    #[test]
    fn resolve_to_pay_md_walks_up_for_sidecar_files() {
        let tmp = tempfile::tempdir().unwrap();
        let provider_dir = tmp.path().join("providers/foo/bar");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::write(provider_dir.join("PAY.md"), "").unwrap();

        // openapi.json sidecar lives next to PAY.md.
        let resolved =
            resolve_to_pay_md(tmp.path(), Path::new("providers/foo/bar/openapi.json")).unwrap();
        assert_eq!(resolved, PathBuf::from("providers/foo/bar/PAY.md"));
    }

    #[test]
    fn resolve_to_pay_md_walks_up_through_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let provider_dir = tmp.path().join("providers/foo/bar");
        std::fs::create_dir_all(provider_dir.join("schemas")).unwrap();
        std::fs::write(provider_dir.join("PAY.md"), "").unwrap();

        let resolved =
            resolve_to_pay_md(tmp.path(), Path::new("providers/foo/bar/schemas/req.json")).unwrap();
        assert_eq!(resolved, PathBuf::from("providers/foo/bar/PAY.md"));
    }

    #[test]
    fn resolve_to_pay_md_returns_none_when_no_pay_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("providers/foo")).unwrap();
        let resolved = resolve_to_pay_md(tmp.path(), Path::new("providers/foo/orphan.txt"));
        assert_eq!(resolved, None);
    }
}
