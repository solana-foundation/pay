//! `pay catalog check` — read-only validation of provider PAY.md files.
//!
//! Three modes, auto-detected from the positional path:
//!   1. **Single-file** (`<path>` is a `.md` / `PAY.md`): parse + frontmatter
//!      validate + probe + Solana verdict. Local devex.
//!   2. **CI diff** (`--changed-from <REF>`): same pipeline, scoped to
//!      provider files changed in the diff. `--format github` emits Actions
//!      annotations. Used by PR CI.
//!   3. **Full registry** (`<path>` is a directory, no `--changed-from`):
//!      walk the registry, run the check pipeline on every provider.
//!
//! Never writes to disk. Use `pay catalog build` to also produce `dist/`.

use std::path::{Path, PathBuf};

use clap::ValueEnum;
use owo_colors::OwoColorize;

use pay_core::skills::probe::{ProbeConfig, ProbeReport};

use super::derive_fqn_from_path;
use super::probe::{
    collect_all_providers, collect_specific_providers, parse_single_provider, render_probe_table,
    run_probe,
};
use super::verdict::{
    ValidationReport, git_changed_provider_files, render_verdict_github, render_verdict_json,
    render_verdict_table, validate_report,
};
use crate::components::{NoticeLevel, print_notice};

/// Output format for verdict + probe data when `--format` is set. Defaults to
/// `table` (human-readable). `json` dumps machine-readable output and
/// suppresses the trailing notice. `github` emits Actions annotations.
#[derive(Debug, Clone, ValueEnum)]
pub enum ReportFormat {
    Table,
    Json,
    Github,
}

#[derive(clap::Args)]
pub struct CheckCommand {
    /// Either a registry root, or a single `PAY.md` / `<name>.md` file. When
    /// pointed at a file, the FQN is derived from the path's parent
    /// directories (e.g. `quicknode/rpc/PAY.md` → fqn `quicknode/rpc`).
    #[arg(default_value = ".")]
    pub path: PathBuf,

    // ── Probe knobs ──────────────────────────────────────────────────────
    /// Skip live probing of endpoints (fast frontmatter-only check).
    #[arg(long)]
    pub no_probe: bool,

    /// Per-endpoint probe timeout in seconds.
    #[arg(long, default_value_t = 10)]
    pub probe_timeout: u64,

    /// Max concurrent provider probes.
    #[arg(long, default_value_t = 5)]
    pub probe_concurrency: usize,

    /// Accepted stablecoin symbols (comma-separated). An endpoint that
    /// advertises a non-listed currency is treated as `wrong_currency` for
    /// verdict purposes.
    #[arg(long, default_value = "USDC,USDT", value_delimiter = ',')]
    pub currencies: Vec<String>,

    // ── Verdict knobs ────────────────────────────────────────────────────
    /// Treat every non-Solana endpoint as a blocking error (default: warn).
    #[arg(long)]
    pub strict: bool,

    /// Verdict output format. `github` emits Actions `::warning::` / `::error::`
    /// annotations; `json` dumps the structured report and suppresses the
    /// trailing notice.
    #[arg(long, default_value = "table", value_enum)]
    pub format: ReportFormat,

    /// Print the per-provider, per-endpoint probe + verdict breakdown in
    /// addition to the summary notice. Helpful when debugging a single
    /// provider; noisy in registry mode.
    #[arg(long, short = 'v')]
    pub verbose: bool,

    // ── CI mode ─────────────────────────────────────────────────────────
    /// Git ref to diff against; only checks `providers/**/*.md` files
    /// changed between `<REF>` and `HEAD`. Used by PR CI.
    #[arg(long, value_name = "REF")]
    pub changed_from: Option<String>,
}

impl CheckCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let canonical = self.path.canonicalize().map_err(|e| {
            pay_core::Error::Config(format!("invalid path `{}`: {e}", self.path.display()))
        })?;

        if canonical.is_file() {
            if self.changed_from.is_some() {
                return Err(pay_core::Error::Config(
                    "--changed-from requires a registry directory, not a single file".into(),
                ));
            }
            return self.run_single_file(&canonical);
        }

        if self.changed_from.is_some() {
            return self.run_changed_from(&canonical);
        }

        self.run_full_registry(&canonical)
    }

    fn probe_config(&self) -> ProbeConfig {
        ProbeConfig {
            accepted_currencies: self.currencies.iter().map(|c| c.to_uppercase()).collect(),
            timeout_secs: self.probe_timeout,
            concurrency: self.probe_concurrency,
        }
    }

    // ── Mode 1: single file ─────────────────────────────────────────────

    fn run_single_file(self, path: &Path) -> pay_core::Result<()> {
        let (fqn, name, operator, origin) = derive_fqn_from_path(path)?;

        // Frontmatter + endpoint validation via the build core. Aborts before
        // probing if the YAML/category/length checks fail — no point probing
        // a file that's syntactically broken.
        let options = pay_core::skills::build::BuildOptions {
            probe: false,
            probe_config: ProbeConfig::default(),
            only: None,
            previous_dist: None,
        };
        let validation_only = pay_core::skills::build::build_single_provider(
            path, &fqn, &name, &operator, &origin, &options,
        );
        if !validation_only.errors.is_empty() {
            print_validation_errors(&validation_only.errors);
            std::process::exit(1);
        }

        let provider = parse_single_provider(path)?;
        let endpoint_count = provider.endpoints.len();

        if self.no_probe {
            print_notice(
                NoticeLevel::Success,
                "PAY.md check successful",
                &format!("{endpoint_count} endpoints walked, probe skipped (--no-probe)"),
            );
            return Ok(());
        }

        let report = run_probe(vec![provider], &self.probe_config());
        let validation = validate_report(&report, self.strict);

        self.render_verbose(&report, &validation);
        self.emit_summary(&report, &validation, endpoint_count, "PAY.md")
    }

    // ── Mode 2: changed-from (PR CI) ────────────────────────────────────

    fn run_changed_from(self, root: &Path) -> pay_core::Result<()> {
        let base_ref = self.changed_from.clone().expect("checked by caller");
        let files = git_changed_provider_files(root, &base_ref)?;
        if files.is_empty() {
            print_notice(
                NoticeLevel::Info,
                "Nothing to check",
                "No changed provider files in the diff.",
            );
            return Ok(());
        }
        let providers = collect_specific_providers(root, &files)?;
        if providers.is_empty() {
            print_notice(
                NoticeLevel::Warning,
                "No matching providers",
                "Diff touched no provider files.",
            );
            return Ok(());
        }

        let total_endpoints: usize = providers.iter().map(|p| p.endpoints.len()).sum();
        if self.no_probe {
            print_notice(
                NoticeLevel::Success,
                "Changed providers check successful",
                &format!(
                    "{total_endpoints} endpoints walked across {} provider{}, probe skipped (--no-probe)",
                    providers.len(),
                    if providers.len() == 1 { "" } else { "s" },
                ),
            );
            return Ok(());
        }
        let report = run_probe(providers, &self.probe_config());
        let validation = validate_report(&report, self.strict);

        self.render_verbose(&report, &validation);
        self.emit_summary(&report, &validation, total_endpoints, "Changed providers")
    }

    // ── Mode 3: full registry (read-only) ──────────────────────────────

    fn run_full_registry(self, root: &Path) -> pay_core::Result<()> {
        let providers = collect_all_providers(root)?;
        if providers.is_empty() {
            print_notice(
                NoticeLevel::Warning,
                "Nothing to check",
                &format!("No provider files under {}/providers/", root.display()),
            );
            return Ok(());
        }

        let total_endpoints: usize = providers.iter().map(|p| p.endpoints.len()).sum();
        if self.no_probe {
            print_notice(
                NoticeLevel::Success,
                "Registry check successful",
                &format!(
                    "{total_endpoints} endpoints walked across {} provider{}, probe skipped (--no-probe)",
                    providers.len(),
                    if providers.len() == 1 { "" } else { "s" },
                ),
            );
            return Ok(());
        }
        let report = run_probe(providers, &self.probe_config());
        let validation = validate_report(&report, self.strict);

        self.render_verbose(&report, &validation);
        self.emit_summary(&report, &validation, total_endpoints, "Registry")
    }

    // ── Shared rendering ────────────────────────────────────────────────

    fn render_verbose(&self, report: &ProbeReport, validation: &ValidationReport) {
        if !self.verbose {
            return;
        }
        if matches!(self.format, ReportFormat::Json | ReportFormat::Github) {
            // Structured formats produce machine output; verbose tables
            // would mix human and machine output on the same stream.
            return;
        }
        eprintln!("{}", "Probe results".bold().underline());
        render_probe_table(report);
        eprintln!("{}", "Solana-compat verdict".bold().underline());
        render_verdict_table(validation);
    }

    fn emit_summary(
        &self,
        report: &ProbeReport,
        validation: &ValidationReport,
        endpoint_count: usize,
        title_prefix: &str,
    ) -> pay_core::Result<()> {
        match self.format {
            ReportFormat::Json => {
                render_verdict_json(validation)?;
                if validation.has_errors() {
                    std::process::exit(1);
                }
                return Ok(());
            }
            ReportFormat::Github => {
                render_verdict_github(validation);
                if validation.has_errors() {
                    std::process::exit(1);
                }
                return Ok(());
            }
            ReportFormat::Table => {}
        }

        let stats = verdict_stats(validation);
        let body = stats.format(endpoint_count, report.failed);

        if validation.has_errors() {
            print_notice(
                NoticeLevel::Error,
                &format!("{title_prefix} check failed"),
                &body,
            );
            std::process::exit(1);
        }
        let warn = !stats.is_clean();
        let level = if warn {
            NoticeLevel::Warning
        } else {
            NoticeLevel::Success
        };
        let title = if warn {
            format!("{title_prefix} check passed with warnings")
        } else {
            format!("{title_prefix} check successful")
        };
        print_notice(level, &title, &body);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct VerdictStats {
    providers: usize,
    blocked: usize,
    ok: usize,
    classified: usize,
    non_solana: usize,
}

impl VerdictStats {
    fn is_clean(&self) -> bool {
        self.blocked == 0 && self.non_solana == 0
    }

    fn format(&self, endpoint_count: usize, probe_failed: usize) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "{} endpoint{} tested across {} provider{}",
            endpoint_count,
            if endpoint_count == 1 { "" } else { "s" },
            self.providers,
            if self.providers == 1 { "" } else { "s" },
        ));
        if self.classified > 0 {
            lines.push(format!(
                "{}/{} gates compatible with Solana",
                self.ok, self.classified
            ));
        }
        if self.non_solana > 0 {
            lines.push(format!(
                "{} non-Solana endpoint{} flagged",
                self.non_solana,
                if self.non_solana == 1 { "" } else { "s" },
            ));
        }
        if self.blocked > 0 {
            lines.push(format!(
                "{} provider{} blocked (zero Solana-compatible gates)",
                self.blocked,
                if self.blocked == 1 { "" } else { "s" },
            ));
        }
        if probe_failed > 0 && self.classified == 0 {
            lines.push(format!("{probe_failed} probe failure(s) — see --verbose"));
        }
        lines.join("\n")
    }
}

fn verdict_stats(validation: &ValidationReport) -> VerdictStats {
    let providers = validation.providers.len();
    let blocked = validation.providers.iter().filter(|p| p.block).count();
    let ok: usize = validation.providers.iter().map(|p| p.ok_count).sum();
    let non_solana: usize = validation
        .providers
        .iter()
        .map(|p| p.non_solana_count)
        .sum();
    let classified = ok + non_solana;
    VerdictStats {
        providers,
        blocked,
        ok,
        classified,
        non_solana,
    }
}

/// Render validation errors through the shared notice component.
pub(super) fn print_validation_errors(errors: &[String]) {
    let title = if errors.len() == 1 {
        "Validation error".to_string()
    } else {
        format!("{} validation errors", errors.len())
    };
    let mut body = String::new();
    for err in errors {
        let mut lines = err.trim_end().lines();
        if let Some(first) = lines.next() {
            body.push_str(&format!("- {first}\n"));
        }
        for line in lines {
            body.push_str(&format!("  {line}\n"));
        }
    }
    print_notice(NoticeLevel::Error, &title, body.trim_end());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(providers: usize, blocked: usize, ok: usize, non_solana: usize) -> VerdictStats {
        VerdictStats {
            providers,
            blocked,
            ok,
            classified: ok + non_solana,
            non_solana,
        }
    }

    #[test]
    fn verdict_stats_clean_when_no_blocks_or_warns() {
        let s = stats(1, 0, 3, 0);
        assert!(s.is_clean());
        assert_eq!(
            s.format(9, 0),
            "9 endpoints tested across 1 provider\n3/3 gates compatible with Solana"
        );
    }

    #[test]
    fn verdict_stats_warns_when_non_solana_present() {
        let s = stats(1, 0, 1, 2);
        assert!(!s.is_clean());
        assert_eq!(
            s.format(9, 0),
            "9 endpoints tested across 1 provider\n\
             1/3 gates compatible with Solana\n\
             2 non-Solana endpoints flagged"
        );
    }

    #[test]
    fn verdict_stats_blocks_when_zero_solana_ok() {
        let s = stats(1, 1, 0, 2);
        assert_eq!(
            s.format(9, 0),
            "9 endpoints tested across 1 provider\n\
             0/2 gates compatible with Solana\n\
             2 non-Solana endpoints flagged\n\
             1 provider blocked (zero Solana-compatible gates)"
        );
    }
}
