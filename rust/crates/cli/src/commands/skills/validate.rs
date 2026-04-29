//! `pay skills validate` — apply Solana-compatibility gates to a probe run.
//!
//! Used by CI on PRs:
//!   - **Warning** for every gated endpoint that doesn't accept Solana
//!     stablecoin payment (e.g. Base-only).
//!   - **Error** when a provider has zero gated endpoints that accept Solana,
//!     i.e. nothing in the diff actually works through pay's wallet.
//!
//! Indeterminate statuses (`siwx_required`, `auth_required`,
//! `unprobeable_needs_body`, `not_found`, …) neither warn nor error — they
//! pass through silently because the probe couldn't classify them.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use clap::ValueEnum;
use owo_colors::OwoColorize;
use serde::Serialize;

use pay_core::skills::probe::{EndpointProbeResult, ProbeConfig, ProbeReport, ProviderProbeResult};

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table.
    Table,
    /// Structured JSON.
    Json,
    /// GitHub Actions `::warning::` / `::error::` annotations.
    Github,
}

/// Validate that changed providers serve at least one Solana-compatible
/// endpoint. Designed for CI on pull requests.
#[derive(clap::Args)]
pub struct ValidateCommand {
    /// Path to the pay-skills registry directory.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Specific provider `.md` files to validate (relative to `path`).
    /// Mutually exclusive with `--changed-from`.
    #[arg(long, num_args = 1..)]
    pub files: Vec<PathBuf>,

    /// Git ref to diff against; validates every `providers/**/*.md` that
    /// changed between `<ref>` and `HEAD`. Typical CI use: `origin/main`.
    #[arg(long, value_name = "REF", conflicts_with = "files")]
    pub changed_from: Option<String>,

    /// Treat every warning as an error (block on first non-Solana endpoint).
    #[arg(long)]
    pub strict: bool,

    /// Accepted stablecoin symbols (comma-separated).
    #[arg(long, default_value = "USDC,USDT", value_delimiter = ',')]
    pub currencies: Vec<String>,

    /// Per-endpoint timeout in seconds.
    #[arg(long, default_value = "10")]
    pub timeout: u64,

    /// Max concurrent provider probes.
    #[arg(long, default_value = "5")]
    pub concurrency: usize,

    /// Output format. `github` is the format CI uses for inline annotations.
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

impl ValidateCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let files = if let Some(ref base) = self.changed_from {
            git_changed_provider_files(&self.path, base)?
        } else if !self.files.is_empty() {
            self.files.clone()
        } else {
            return Err(pay_core::Error::Config(
                "specify either --files or --changed-from".into(),
            ));
        };

        if files.is_empty() {
            eprintln!("{}", "No changed provider files to validate.".dimmed());
            return Ok(());
        }

        let config = ProbeConfig {
            accepted_currencies: self.currencies.iter().map(|c| c.to_uppercase()).collect(),
            timeout_secs: self.timeout,
            concurrency: self.concurrency,
        };

        let providers = super::probe::collect_specific_providers(&self.path, &files)?;
        if providers.is_empty() {
            eprintln!("{}", "No matching providers found.".yellow());
            return Ok(());
        }

        let total_eps: usize = providers.iter().map(|p| p.endpoints.len()).sum();
        eprintln!(
            "Validating {} provider(s) ({} endpoints) against Solana stables [{}]...",
            providers.len().to_string().bold(),
            total_eps.to_string().bold(),
            self.currencies.join(", ").dimmed(),
        );
        eprintln!();

        let report = pay_core::skills::probe::probe_providers(providers, &config);
        let validation = validate_report(&report, self.strict);

        match self.format {
            OutputFormat::Table => render_table(&validation),
            OutputFormat::Json => render_json(&validation)?,
            OutputFormat::Github => render_github(&validation),
        }

        if validation.has_errors() {
            std::process::exit(1);
        }
        Ok(())
    }
}

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
        .map(|p| validate_provider(p, strict))
        .collect();
    ValidationReport { providers, strict }
}

fn validate_provider(provider: &ProviderProbeResult, strict: bool) -> ProviderVerdict {
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
    // FQN matches the relative path under providers/, sans the `.md`
    // extension (e.g. `merit-systems/stabledomains/domains` →
    // `providers/merit-systems/stabledomains/domains.md`).
    format!("providers/{fqn}.md")
}

// ── git-diff plumbing ───────────────────────────────────────────────────────

fn git_changed_provider_files(repo_root: &Path, base_ref: &str) -> pay_core::Result<Vec<PathBuf>> {
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
    let files: Vec<PathBuf> = stdout
        .lines()
        .filter(|line| line.starts_with("providers/") && line.ends_with(".md"))
        .map(PathBuf::from)
        .collect();
    Ok(files)
}

// ── renderers ──────────────────────────────────────────────────────────────

fn render_table(report: &ValidationReport) {
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
    eprintln!();
    if block_count > 0 {
        eprintln!(
            "{} {} provider(s), {} non-Solana endpoint warning(s)",
            "Validation failed:".red().bold(),
            block_count.to_string().red(),
            warn_count
        );
    } else if warn_count > 0 {
        eprintln!(
            "{} {} non-Solana endpoint(s) flagged",
            "Validation passed with warnings:".yellow().bold(),
            warn_count
        );
    } else {
        eprintln!("{}", "Validation passed.".green().bold());
    }
}

fn render_json(report: &ValidationReport) -> pay_core::Result<()> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
    println!("{json}");
    Ok(())
}

fn render_github(report: &ValidationReport) {
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
    fn provider_md_path_drops_md_suffix() {
        assert_eq!(
            provider_md_path("merit-systems/stabledomains/domains"),
            "providers/merit-systems/stabledomains/domains.md"
        );
    }

    #[test]
    fn encode_actions_escapes_percent_and_newlines() {
        assert_eq!(encode_actions("a%b\nc\rd"), "a%25b%0Ac%0Dd");
    }
}
