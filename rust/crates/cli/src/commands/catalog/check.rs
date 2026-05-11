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

use pay_core::skills::build::{BuildOptions, BuildResult};
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

    /// Also write a GitHub-flavored markdown summary to this path
    /// (regardless of `--format`). Lets PR CI emit inline annotations
    /// *and* a tidy step-summary table from a single probe run, instead
    /// of re-probing every endpoint just to get a different output shape.
    /// Workflows append this file to `$GITHUB_STEP_SUMMARY`.
    #[arg(long, value_name = "PATH")]
    pub summary_out: Option<PathBuf>,

    // ── Diff modes ──────────────────────────────────────────────────────
    /// Specific provider PAY.md files to check (relative to the registry root
    /// when `path` is a directory, absolute otherwise). Used by CI to pass a
    /// pre-computed list without requiring `git` in the runtime container.
    /// Mutually exclusive with `--changed-from`.
    #[arg(long, value_name = "PATH", num_args = 1.., conflicts_with = "changed_from")]
    pub files: Vec<PathBuf>,

    /// Git ref to diff against; checks providers whose `PAY.md` (or any
    /// sidecar under their directory) changed between `<REF>` and `HEAD`.
    /// Local-devex shortcut — requires `git` on `$PATH`. CI should use
    /// `--files` instead.
    #[arg(long, value_name = "REF")]
    pub changed_from: Option<String>,
}

impl CheckCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let canonical = self.path.canonicalize().map_err(|e| {
            pay_core::Error::Config(format!("invalid path `{}`: {e}", self.path.display()))
        })?;

        if canonical.is_file() {
            if self.changed_from.is_some() || !self.files.is_empty() {
                return Err(pay_core::Error::Config(
                    "--changed-from / --files require a registry directory, not a single file"
                        .into(),
                ));
            }
            return self.run_single_file(&canonical);
        }

        if !self.files.is_empty() {
            return self.run_explicit_files(&canonical);
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
        let validation_only = pay_core::skills::build::build_single_provider(
            path,
            &fqn,
            &name,
            &operator,
            &origin,
            &static_build_options(),
        );
        let static_validation = self.handle_static_validation(validation_only);

        let provider = parse_single_provider(path)?;
        let endpoint_count = static_validation
            .endpoint_count
            .max(provider.endpoints.len());

        if self.no_probe {
            self.render_static_warnings(&static_validation.warnings);
            let level = if static_validation.warnings.is_empty() {
                NoticeLevel::Success
            } else {
                NoticeLevel::Warning
            };
            let title = if static_validation.warnings.is_empty() {
                "PAY.md check successful"
            } else {
                "PAY.md check passed with warnings"
            };
            print_notice(
                level,
                title,
                &format!(
                    "{endpoint_count} endpoints walked, probe skipped (--no-probe){}",
                    warning_suffix(static_validation.warnings.len())
                ),
            );
            return Ok(());
        }

        let report = run_probe(vec![provider], &self.probe_config());
        let validation = validate_report(&report, self.strict);

        self.render_static_warnings(&static_validation.warnings);
        self.render_verbose(&report, &validation);
        self.emit_summary(
            &report,
            &validation,
            endpoint_count,
            &static_validation.warnings,
            "PAY.md",
        )
    }

    // ── Mode 2a: explicit list of paths (CI) ────────────────────────────

    fn run_explicit_files(self, root: &Path) -> pay_core::Result<()> {
        let files = self.files.clone();
        self.run_with_paths(root, &files, "Changed providers")
    }

    // ── Mode 2b: changed-from (local devex; requires git on PATH) ───────

    fn run_changed_from(self, root: &Path) -> pay_core::Result<()> {
        let base_ref = self.changed_from.clone().expect("checked by caller");
        let files = git_changed_provider_files(root, &base_ref)?;
        self.run_with_paths(root, &files, "Changed providers")
    }

    /// Probe + verdict over a pre-computed list of provider PAY.md paths.
    /// Shared by `--files` and `--changed-from`.
    fn run_with_paths(
        self,
        root: &Path,
        files: &[PathBuf],
        title_prefix: &str,
    ) -> pay_core::Result<()> {
        if files.is_empty() {
            print_notice(
                NoticeLevel::Info,
                "Nothing to check",
                "No changed provider files.",
            );
            return Ok(());
        }
        let static_validation = self.static_check_paths(root, files)?;
        let providers = collect_specific_providers(root, files)?;
        if providers.is_empty() {
            print_notice(
                NoticeLevel::Warning,
                "No matching providers",
                "Provided paths did not resolve to any registered providers.",
            );
            return Ok(());
        }

        let total_endpoints: usize = providers.iter().map(|p| p.endpoints.len()).sum();
        if self.no_probe {
            self.render_static_warnings(&static_validation.warnings);
            let level = if static_validation.warnings.is_empty() {
                NoticeLevel::Success
            } else {
                NoticeLevel::Warning
            };
            let title = if static_validation.warnings.is_empty() {
                format!("{title_prefix} check successful")
            } else {
                format!("{title_prefix} check passed with warnings")
            };
            let body = format!(
                "{total_endpoints} endpoints walked across {} provider{}, probe skipped (--no-probe){}",
                providers.len(),
                if providers.len() == 1 { "" } else { "s" },
                warning_suffix(static_validation.warnings.len()),
            );
            print_notice(level, &title, &body);
            return Ok(());
        }
        let report = run_probe(providers, &self.probe_config());
        let validation = validate_report(&report, self.strict);

        self.render_static_warnings(&static_validation.warnings);
        self.render_verbose(&report, &validation);
        self.emit_summary(
            &report,
            &validation,
            total_endpoints,
            &static_validation.warnings,
            title_prefix,
        )
    }

    // ── Mode 3: full registry (read-only) ──────────────────────────────

    fn run_full_registry(self, root: &Path) -> pay_core::Result<()> {
        let static_validation = self.static_check_registry(root);
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
            self.render_static_warnings(&static_validation.warnings);
            let level = if static_validation.warnings.is_empty() {
                NoticeLevel::Success
            } else {
                NoticeLevel::Warning
            };
            let title = if static_validation.warnings.is_empty() {
                "Registry check successful"
            } else {
                "Registry check passed with warnings"
            };
            let body = format!(
                "{total_endpoints} endpoints walked across {} provider{}, probe skipped (--no-probe){}",
                providers.len(),
                if providers.len() == 1 { "" } else { "s" },
                warning_suffix(static_validation.warnings.len()),
            );
            print_notice(level, title, &body);
            return Ok(());
        }
        let report = run_probe(providers, &self.probe_config());
        let validation = validate_report(&report, self.strict);

        self.render_static_warnings(&static_validation.warnings);
        self.render_verbose(&report, &validation);
        self.emit_summary(
            &report,
            &validation,
            total_endpoints,
            &static_validation.warnings,
            "Registry",
        )
    }

    // ── Static validation ───────────────────────────────────────────────

    fn static_check_registry(&self, root: &Path) -> StaticValidation {
        let result = pay_core::skills::build::build_with_options(
            root,
            "",
            String::new(),
            &static_build_options(),
        );
        self.handle_static_validation(result)
    }

    fn static_check_paths(
        &self,
        root: &Path,
        files: &[PathBuf],
    ) -> pay_core::Result<StaticValidation> {
        let mut combined = StaticValidation::default();
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        for file in files {
            let full_path = if file.is_absolute() {
                file.clone()
            } else {
                root.join(file)
            };
            if !full_path.exists() {
                warnings.push(format!("{}: file not found, skipped", file.display()));
                continue;
            }
            let canonical = full_path.canonicalize().map_err(|e| {
                pay_core::Error::Config(format!("invalid file `{}`: {e}", full_path.display()))
            })?;
            let (fqn, name, operator, origin) = derive_provider_identity(root, &canonical)?;
            let result = pay_core::skills::build::build_single_provider(
                &canonical,
                &fqn,
                &name,
                &operator,
                &origin,
                &static_build_options(),
            );
            combined.provider_count += result.index.provider_count;
            combined.endpoint_count += result
                .index
                .providers
                .iter()
                .map(|provider| provider.endpoint_count)
                .sum::<usize>();
            errors.extend(result.errors);
            warnings.extend(result.warnings);
        }

        if !errors.is_empty() {
            self.render_static_warnings(&warnings);
            self.render_static_errors(&errors);
            std::process::exit(1);
        }
        combined.warnings = warnings;
        Ok(combined)
    }

    fn handle_static_validation(&self, result: BuildResult) -> StaticValidation {
        if !result.errors.is_empty() {
            self.render_static_warnings(&result.warnings);
            self.render_static_errors(&result.errors);
            std::process::exit(1);
        }
        StaticValidation {
            provider_count: result.index.provider_count,
            endpoint_count: result
                .index
                .providers
                .iter()
                .map(|provider| provider.endpoint_count)
                .sum(),
            warnings: result.warnings,
        }
    }

    fn render_static_errors(&self, errors: &[String]) {
        if matches!(self.format, ReportFormat::Github) {
            render_github_static_findings("error", errors);
        } else {
            print_validation_errors(errors);
        }
    }

    fn render_static_warnings(&self, warnings: &[String]) {
        if warnings.is_empty() {
            return;
        }
        match self.format {
            ReportFormat::Github => render_github_static_findings("warning", warnings),
            ReportFormat::Json => {}
            ReportFormat::Table => print_validation_warnings(warnings),
        }
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
        static_warnings: &[String],
        title_prefix: &str,
    ) -> pay_core::Result<()> {
        // Optional markdown sidecar — written before any potential exit
        // so the file is on disk even when validation blocks. PR CI cats
        // this into `$GITHUB_STEP_SUMMARY`.
        if let Some(path) = &self.summary_out {
            let body = render_markdown_summary(validation, static_warnings);
            std::fs::write(path, body)
                .map_err(|e| pay_core::Error::Config(format!("write {}: {e}", path.display())))?;
        }

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
        let static_warning_count = static_warnings.len();
        let body = stats.format(endpoint_count, report.failed, static_warning_count);

        if validation.has_errors() {
            print_notice(
                NoticeLevel::Error,
                &format!("{title_prefix} check failed"),
                &body,
            );
            std::process::exit(1);
        }
        let warn = !stats.is_clean() || static_warning_count > 0;
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

#[derive(Debug, Default)]
struct StaticValidation {
    provider_count: usize,
    endpoint_count: usize,
    warnings: Vec<String>,
}

fn static_build_options() -> BuildOptions {
    BuildOptions {
        probe: false,
        probe_config: ProbeConfig::default(),
        only: None,
        previous_dist: None,
    }
}

fn warning_suffix(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!(
            "\n{count} static validation warning{}",
            if count == 1 { "" } else { "s" }
        )
    }
}

fn derive_provider_identity(
    root: &Path,
    path: &Path,
) -> pay_core::Result<(String, String, String, String)> {
    let providers_root = root.join("providers");
    let provider_dir = path.parent().unwrap_or(path);
    let rel = match provider_dir.strip_prefix(&providers_root) {
        Ok(rel) => rel,
        Err(_) => return derive_fqn_from_path(path),
    };
    let segments: Vec<String> = rel
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(String::from))
        .filter(|segment| !segment.is_empty() && segment != "." && segment != "..")
        .collect();
    if segments.is_empty() {
        return Err(pay_core::Error::Config(format!(
            "{} has no provider path under {}/providers",
            path.display(),
            root.display()
        )));
    }
    let fqn = segments.join("/");
    let name = segments.last().unwrap().clone();
    let operator = segments.first().unwrap().clone();
    let origin = if segments.len() >= 3 {
        segments[segments.len() - 2].clone()
    } else {
        operator.clone()
    };
    Ok((fqn, name, operator, origin))
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

    fn format(
        &self,
        endpoint_count: usize,
        probe_failed: usize,
        static_warning_count: usize,
    ) -> String {
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
        if static_warning_count > 0 {
            lines.push(format!(
                "{static_warning_count} static validation warning{}",
                if static_warning_count == 1 { "" } else { "s" },
            ));
        }
        lines.join("\n")
    }
}

/// Render a GitHub-flavored markdown step-summary for a verdict report.
/// Mirrors what the previous workflow generated via Python — top-line
/// status, per-provider table, and per-provider details for non-Solana
/// endpoints.
fn render_markdown_summary(validation: &ValidationReport, static_warnings: &[String]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "### Solana-compatibility verdict");
    let _ = writeln!(out);

    let blocks = validation.providers.iter().filter(|p| p.block).count();
    let warns: usize = validation
        .providers
        .iter()
        .map(|p| p.non_solana_count)
        .sum();
    let oks: usize = validation.providers.iter().map(|p| p.ok_count).sum();

    if blocks > 0 {
        let _ = writeln!(out, ":no_entry: **{blocks}** provider(s) blocked");
    } else if warns > 0 {
        let _ = writeln!(out, ":warning: {warns} non-Solana endpoint(s) flagged");
    } else {
        let _ = writeln!(
            out,
            ":white_check_mark: {oks} Solana-compatible endpoint(s)"
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "| Provider | OK | Non-Solana | Verdict |");
    let _ = writeln!(out, "|----------|----|------------|---------|");
    for p in &validation.providers {
        let v = if p.block {
            ":no_entry: BLOCK"
        } else if p.non_solana_count > 0 {
            ":warning: warn"
        } else {
            ":white_check_mark: pass"
        };
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            p.fqn, p.ok_count, p.non_solana_count, v
        );
    }
    for p in &validation.providers {
        if p.non_solana_count == 0 {
            continue;
        }
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "<details><summary>{} — non-Solana endpoints</summary>",
            p.fqn
        );
        let _ = writeln!(out);
        let _ = writeln!(out, "| Method | Path | Status | Note |");
        let _ = writeln!(out, "|--------|------|--------|------|");
        for ep in &p.endpoints {
            if !matches!(ep.verdict, super::verdict::EndpointVerdict::NotSolana) {
                continue;
            }
            let _ = writeln!(
                out,
                "| `{}` | `{}` | `{}` | {} |",
                ep.method, ep.path, ep.probe_status, ep.note
            );
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "</details>");
    }
    if !static_warnings.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "### Static validation warnings");
        let _ = writeln!(out);
        for warning in static_warnings {
            let first_line = warning.lines().next().unwrap_or(warning);
            let _ = writeln!(out, "- `{}`", escape_markdown_inline(first_line));
        }
    }
    out
}

fn escape_markdown_inline(value: &str) -> String {
    value.replace('`', "\\`")
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

fn print_validation_warnings(warnings: &[String]) {
    let title = if warnings.len() == 1 {
        "Validation warning".to_string()
    } else {
        format!("{} validation warnings", warnings.len())
    };
    let mut body = String::new();
    for warning in warnings {
        let mut lines = warning.trim_end().lines();
        if let Some(first) = lines.next() {
            body.push_str(&format!("- {first}\n"));
        }
        for line in lines {
            body.push_str(&format!("  {line}\n"));
        }
    }
    print_notice(NoticeLevel::Warning, &title, body.trim_end());
}

fn render_github_static_findings(kind: &str, findings: &[String]) {
    for finding in findings {
        println!(
            "::{kind} title={title}::{message}",
            title = encode_actions("pay-skills static validation"),
            message = encode_actions(finding),
        );
    }
}

fn encode_actions(msg: &str) -> String {
    msg.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
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
            s.format(9, 0, 0),
            "9 endpoints tested across 1 provider\n3/3 gates compatible with Solana"
        );
    }

    #[test]
    fn verdict_stats_warns_when_non_solana_present() {
        let s = stats(1, 0, 1, 2);
        assert!(!s.is_clean());
        assert_eq!(
            s.format(9, 0, 0),
            "9 endpoints tested across 1 provider\n\
             1/3 gates compatible with Solana\n\
             2 non-Solana endpoints flagged"
        );
    }

    #[test]
    fn verdict_stats_blocks_when_zero_solana_ok() {
        let s = stats(1, 1, 0, 2);
        assert_eq!(
            s.format(9, 0, 0),
            "9 endpoints tested across 1 provider\n\
             0/2 gates compatible with Solana\n\
             2 non-Solana endpoints flagged\n\
             1 provider blocked (zero Solana-compatible gates)"
        );
    }

    #[test]
    fn verdict_stats_includes_static_warnings() {
        let s = stats(1, 0, 1, 0);
        assert_eq!(
            s.format(1, 0, 2),
            "1 endpoint tested across 1 provider\n\
             1/1 gates compatible with Solana\n\
             2 static validation warnings"
        );
    }
}
