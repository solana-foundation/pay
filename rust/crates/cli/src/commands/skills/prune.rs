//! `pay skills prune-merged` — drop pinned providers whose PRs have
//! been merged upstream.
//!
//! The canonical catalog will pick up the provider on the next
//! `pay skills update`, so the pin is no longer load-bearing. Branch
//! and SHA pins are immune by design (no merge concept).

use std::io::IsTerminal;

use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;
use owo_colors::OwoColorize;

use pay_core::skills::github;
use pay_core::skills::pin::{PinAnchor, PinManifest, PinStore};

/// Prune pinned providers whose PRs are merged upstream.
#[derive(clap::Args)]
pub struct PruneMergedCommand {
    /// Don't prompt — assume yes. Required in non-TTY contexts (CI).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Only print what would be pruned; make no changes.
    #[arg(long)]
    pub dry_run: bool,
}

/// One pin's classification after the network probe.
enum Verdict {
    /// PR-anchored pin that's merged upstream → prune candidate.
    Merged,
    /// PR-anchored pin still open → keep.
    Open,
    /// Non-PR pin (branch / sha) — no merge concept → keep.
    NotApplicable,
    /// GitHub call failed → keep (don't penalize transient errors).
    Error(String),
}

struct Classified {
    manifest: PinManifest,
    verdict: Verdict,
}

impl PruneMergedCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let store = PinStore::open_default();
        let pins = store.read_all();
        if pins.is_empty() {
            eprintln!("{}", "  No pinned providers.".dimmed());
            return Ok(());
        }

        eprintln!("  {} {} pinned providers...", "Checking".cyan(), pins.len());
        let classified: Vec<Classified> = pins
            .into_iter()
            .map(|(manifest, _)| {
                let verdict = classify(&manifest);
                eprintln!("    {}", format_row(&manifest, &verdict));
                Classified { manifest, verdict }
            })
            .collect();

        let to_prune: Vec<&PinManifest> = classified
            .iter()
            .filter(|c| matches!(c.verdict, Verdict::Merged))
            .map(|c| &c.manifest)
            .collect();

        eprintln!();
        if to_prune.is_empty() {
            eprintln!("  {} nothing to prune.", "✓".green());
            return Ok(());
        }

        eprintln!(
            "  {} {} {} to prune:",
            "Found".yellow(),
            to_prune.len(),
            if to_prune.len() == 1 { "pin" } else { "pins" }
        );
        for m in &to_prune {
            eprintln!(
                "    - {} ({}, {})",
                m.fqn.bold(),
                m.anchor_label(),
                m.short_sha().dimmed()
            );
        }

        if self.dry_run {
            eprintln!();
            eprintln!("  {}", "Dry run — no changes made.".dimmed());
            return Ok(());
        }

        if !self.yes && !confirm(to_prune.len())? {
            eprintln!("{}", "  Cancelled.".dimmed());
            return Ok(());
        }

        let mut removed = 0;
        let mut failed = Vec::new();
        for m in &to_prune {
            match store.remove(&m.fqn) {
                Ok(true) => removed += 1,
                Ok(false) => { /* concurrent remove, treat as ok */ }
                Err(e) => failed.push((m.fqn.clone(), e.to_string())),
            }
        }
        eprintln!();
        eprintln!("  {} {} pruned", "Done:".green(), removed);
        if !failed.is_empty() {
            for (fqn, err) in &failed {
                eprintln!("    {} {}: {}", "✘".red(), fqn, err);
            }
            return Err(pay_core::Error::Config(format!(
                "{} pin(s) could not be removed",
                failed.len()
            )));
        }
        Ok(())
    }
}

/// Probe GitHub for the current merged status of a pin. PR pins are
/// the only ones with a merge concept.
fn classify(manifest: &PinManifest) -> Verdict {
    match &manifest.anchor {
        PinAnchor::Pr { pr, .. } => match github::resolve_pr(&manifest.source_repo, *pr) {
            Ok(info) if info.merged => Verdict::Merged,
            Ok(_) => Verdict::Open,
            Err(e) => Verdict::Error(e.to_string()),
        },
        PinAnchor::Branch { .. } | PinAnchor::Sha => Verdict::NotApplicable,
    }
}

fn format_row(manifest: &PinManifest, verdict: &Verdict) -> String {
    let head = format!("{:<32} {}", manifest.fqn, manifest.anchor_label(),);
    match verdict {
        Verdict::Merged => format!("{}  {} {}", head, "→".dimmed(), "merged ✓".green()),
        Verdict::Open => format!("{}  {} {}", head, "→".dimmed(), "open".dimmed()),
        Verdict::NotApplicable => format!("{}  {}", head, "(no merge concept)".dimmed()),
        Verdict::Error(e) => format!("{}  {} {}", head, "✘".red(), e.dimmed()),
    }
}

fn confirm(n: usize) -> pay_core::Result<bool> {
    let tty = std::io::stderr().is_terminal() && std::io::stdin().is_terminal();
    if !tty {
        return Err(pay_core::Error::Config(format!(
            "refusing to prune {n} pin(s) in non-TTY context; pass --yes or --dry-run"
        )));
    }
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Prune {n} pin(s)?"))
        .default(false) // destructive — opt-in
        .interact()
        .map_err(|e| pay_core::Error::Config(format!("prompt failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_pr() -> PinManifest {
        PinManifest {
            fqn: "venice/ai".into(),
            source_repo: "solana-foundation/pay-skills".into(),
            head_repo: "fork/pay-skills".into(),
            anchor: PinAnchor::Pr {
                pr: 137,
                head_ref: "feat/x".into(),
            },
            sha: "abc1234".into(),
            installed_at: "2026-06-12T00:00:00Z".into(),
            merged: false,
            files: vec![],
        }
    }

    #[test]
    fn format_row_merged() {
        let row = format_row(&manifest_pr(), &Verdict::Merged);
        assert!(row.contains("venice/ai"));
        assert!(row.contains("PR 137"));
        assert!(row.contains("merged"));
    }

    #[test]
    fn format_row_open() {
        let row = format_row(&manifest_pr(), &Verdict::Open);
        assert!(row.contains("open"));
    }

    #[test]
    fn format_row_branch_not_applicable() {
        let mut m = manifest_pr();
        m.anchor = PinAnchor::Branch {
            ref_name: "experimental".into(),
        };
        let row = format_row(&m, &Verdict::NotApplicable);
        assert!(row.contains("branch experimental"));
        assert!(row.contains("no merge concept"));
    }

    #[test]
    fn format_row_error_surfaces_message() {
        let row = format_row(&manifest_pr(), &Verdict::Error("403 forbidden".into()));
        assert!(row.contains("403 forbidden"));
    }
}
