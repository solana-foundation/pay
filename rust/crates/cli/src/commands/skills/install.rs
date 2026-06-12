//! `pay skills add` — adds a catalog source OR pins a single provider
//! from a specific PR / branch / SHA.
//!
//! Two modes:
//!
//! **Source mode** (default): `pay skills add company/apis` appends a
//! catalog source to `~/.config/pay/skills.yaml` so its providers
//! union into the catalog on subsequent loads. Backward-compatible
//! with the original surface.
//!
//! **Pin mode** (`--pr` / `--branch` / `--sha`): treats the positional
//! arg as a provider FQN inside the chosen catalog repo (default
//! `solana-foundation/pay-skills`), fetches just that provider's
//! directory, and installs it into the overlay store. The overlay
//! shadows the canonical catalog so the user sees their pinned version
//! when they run `pay curl`, `pay skills search`, etc. Designed for
//! testing a PR end-to-end before it lands.

use std::io::IsTerminal;

use dialoguer::theme::ColorfulTheme;
use dialoguer::Confirm;
use owo_colors::OwoColorize;

use pay_core::skills::github::{self, PrInfo};
use pay_core::skills::pin::{PinAnchor, PinManifest, PinStore};

const DEFAULT_REPO: &str = "solana-foundation/pay-skills";

/// Add a provider source to the skills catalog, or pin a single
/// provider from a specific PR / branch / SHA.
#[derive(clap::Args)]
pub struct InstallCommand {
    /// In source mode: GitHub `org/repo` or a full catalog URL.
    /// In pin mode (`--pr`/`--branch`/`--sha`): the provider FQN
    /// (e.g. `venice/ai`) inside the chosen catalog repo.
    pub source: String,

    /// Pin: pull request number on the catalog repo.
    #[arg(long, conflicts_with_all = ["branch", "sha"])]
    pub pr: Option<u32>,

    /// Pin: branch name on the catalog repo.
    #[arg(long, conflicts_with_all = ["pr", "sha"])]
    pub branch: Option<String>,

    /// Pin: a specific commit SHA on the catalog repo (immutable anchor).
    #[arg(long, conflicts_with_all = ["pr", "branch"])]
    pub sha: Option<String>,

    /// Pin: override the catalog repo. Defaults to
    /// `solana-foundation/pay-skills`.
    #[arg(long, requires_ifs = [("Some(_)", "pr"), ("Some(_)", "branch"), ("Some(_)", "sha")])]
    pub repo: Option<String>,

    /// Pin: overwrite any conflicting pin without prompting. Required
    /// in non-TTY contexts (CI) when a conflict would otherwise prompt.
    #[arg(long)]
    pub force: bool,
}

impl InstallCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let is_pin = self.pr.is_some() || self.branch.is_some() || self.sha.is_some();
        if is_pin {
            self.run_pin()
        } else {
            self.run_source()
        }
    }

    /// Original behavior — append a catalog source to skills.yaml.
    fn run_source(self) -> pay_core::Result<()> {
        let mut cfg = pay_core::skills::config::SkillsConfig::load()?;
        if cfg.add_source(&self.source) {
            cfg.save()?;
            eprintln!("  {} {}", "Added:".green(), self.source);
            eprintln!("{}", "  Updating cache...".dimmed());
            let catalog = pay_core::skills::blocking::update_skills(false)?;
            eprintln!(
                "  {} {} providers",
                "Ready:".green(),
                catalog.providers.len(),
            );
        } else {
            eprintln!("{}", "  Already installed.".dimmed());
        }
        Ok(())
    }

    /// Pin a single provider FQN from a PR/branch/SHA.
    fn run_pin(self) -> pay_core::Result<()> {
        let repo = self.repo.clone().unwrap_or_else(|| DEFAULT_REPO.to_string());
        let fqn = self.source.clone();
        let store = PinStore::open_default();
        let existing = store.get(&fqn)?;

        // Resolve the user's anchor → concrete head SHA + repo + merged flag.
        let resolved = resolve_anchor(&repo, &self)?;
        let new_anchor = build_anchor(&self, &resolved);

        // Conflict matrix per the design doc.
        if let Some(existing_pin) = &existing {
            match classify_conflict(existing_pin, &new_anchor, &resolved.head_sha) {
                Conflict::SameAnchorSameSha => {
                    eprintln!(
                        "  {} {} already at {} ({})",
                        "✓".green(),
                        fqn,
                        existing_pin.anchor_label(),
                        existing_pin.short_sha(),
                    );
                    return Ok(());
                }
                Conflict::SameAnchorMovedSha => {
                    eprintln!(
                        "  {} {} moved {} → {}; refreshing...",
                        "↻".cyan(),
                        existing_pin.anchor_label(),
                        existing_pin.short_sha(),
                        short_sha(&resolved.head_sha),
                    );
                }
                Conflict::CrossKindOrAnchor { default_yes } => {
                    if !self.force && !confirm_replace(existing_pin, &new_anchor, default_yes)? {
                        eprintln!("{}", "  Cancelled.".dimmed());
                        return Ok(());
                    }
                }
            }
        }

        // Shadow notice — the canonical catalog might have this FQN. Best-effort
        // probe of the current cache; loading the catalog fresh would be too
        // expensive for an interactive command.
        if existing.is_none()
            && let Ok(catalog) = pay_core::skills::load_cached_skills()
            && catalog.providers.iter().any(|p| p.fqn == fqn)
        {
            eprintln!(
                "  {} shadowing canonical {} from {}",
                "ℹ".cyan(),
                fqn,
                repo,
            );
        }

        // Fetch the directory tree + every blob.
        let prefix = format!("providers/{fqn}");
        eprintln!(
            "  {} {} from {}@{}",
            "Fetching".cyan(),
            prefix,
            resolved.head_repo,
            short_sha(&resolved.head_sha),
        );
        let entries = github::list_directory(&resolved.head_repo, &resolved.head_sha, &prefix)?;
        if entries.is_empty() {
            return Err(pay_core::Error::Config(format!(
                "no files under {prefix} at {}@{}",
                resolved.head_repo,
                short_sha(&resolved.head_sha)
            )));
        }
        let mut files: Vec<(String, Vec<u8>)> = Vec::with_capacity(entries.len());
        for entry in &entries {
            let bytes = github::fetch_blob(&resolved.head_repo, &entry.sha)?;
            files.push((entry.path.clone(), bytes));
        }

        let mut manifest = PinManifest {
            fqn: fqn.clone(),
            source_repo: repo.clone(),
            head_repo: resolved.head_repo.clone(),
            anchor: new_anchor.clone(),
            sha: resolved.head_sha.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            merged: resolved.merged,
            files: Vec::new(), // upsert() fills this in
        };
        store.upsert(&mut manifest, &files)?;

        let kind = match &new_anchor {
            PinAnchor::Pr { pr, .. } => format!("PR {pr}"),
            PinAnchor::Branch { ref_name } => format!("branch {ref_name}"),
            PinAnchor::Sha => format!("sha {}", short_sha(&resolved.head_sha)),
        };
        eprintln!(
            "  {} {} from {} ({} files, {})",
            "Pinned".green(),
            fqn,
            kind,
            manifest.files.len(),
            short_sha(&resolved.head_sha),
        );
        if resolved.merged && matches!(new_anchor, PinAnchor::Pr { .. }) {
            eprintln!(
                "  {} this PR is already merged — `pay skills update` or `pay skills remove {fqn}` to drop the pin",
                "ℹ".cyan(),
            );
        }
        Ok(())
    }
}

/// Resolved head info, regardless of which anchor flag the user passed.
struct ResolvedAnchor {
    head_repo: String,
    head_sha: String,
    head_ref: Option<String>,
    merged: bool,
}

fn resolve_anchor(repo: &str, cmd: &InstallCommand) -> pay_core::Result<ResolvedAnchor> {
    if let Some(pr) = cmd.pr {
        let info: PrInfo = github::resolve_pr(repo, pr)?;
        Ok(ResolvedAnchor {
            head_repo: info.head_repo,
            head_sha: info.head_sha,
            head_ref: Some(info.head_ref),
            merged: info.merged,
        })
    } else if let Some(branch) = &cmd.branch {
        let sha = github::resolve_branch(repo, branch)?;
        Ok(ResolvedAnchor {
            head_repo: repo.to_string(),
            head_sha: sha,
            head_ref: Some(branch.clone()),
            merged: false,
        })
    } else if let Some(sha) = &cmd.sha {
        Ok(ResolvedAnchor {
            head_repo: repo.to_string(),
            head_sha: sha.clone(),
            head_ref: None,
            merged: false,
        })
    } else {
        unreachable!("run_pin called without any anchor flag")
    }
}

fn build_anchor(cmd: &InstallCommand, resolved: &ResolvedAnchor) -> PinAnchor {
    if let Some(pr) = cmd.pr {
        PinAnchor::Pr {
            pr,
            head_ref: resolved.head_ref.clone().unwrap_or_default(),
        }
    } else if let Some(branch) = &cmd.branch {
        PinAnchor::Branch {
            ref_name: branch.clone(),
        }
    } else {
        PinAnchor::Sha
    }
}

enum Conflict {
    /// Same anchor, identical SHA — no-op.
    SameAnchorSameSha,
    /// Same anchor (e.g. PR 137), SHA moved — refresh quietly.
    SameAnchorMovedSha,
    /// Different anchor or different anchor kind. Prompt; `default_yes`
    /// is true when same kind (e.g. PR→PR), false when crossing kinds
    /// (branch→PR) which is more deliberate.
    CrossKindOrAnchor { default_yes: bool },
}

fn classify_conflict(existing: &PinManifest, incoming: &PinAnchor, incoming_sha: &str) -> Conflict {
    let same_anchor = match (&existing.anchor, incoming) {
        (PinAnchor::Pr { pr: a, .. }, PinAnchor::Pr { pr: b, .. }) => a == b,
        (PinAnchor::Branch { ref_name: a }, PinAnchor::Branch { ref_name: b }) => a == b,
        (PinAnchor::Sha, PinAnchor::Sha) => true,
        _ => false,
    };
    if same_anchor {
        if existing.sha == incoming_sha {
            return Conflict::SameAnchorSameSha;
        }
        return Conflict::SameAnchorMovedSha;
    }
    let cross_kind = !matches!(
        (&existing.anchor, incoming),
        (PinAnchor::Pr { .. }, PinAnchor::Pr { .. })
            | (PinAnchor::Branch { .. }, PinAnchor::Branch { .. })
            | (PinAnchor::Sha, PinAnchor::Sha)
    );
    Conflict::CrossKindOrAnchor {
        default_yes: !cross_kind,
    }
}

fn confirm_replace(
    existing: &PinManifest,
    incoming: &PinAnchor,
    default_yes: bool,
) -> pay_core::Result<bool> {
    let tty = std::io::stderr().is_terminal() && std::io::stdin().is_terminal();
    if !tty {
        return Err(pay_core::Error::Config(format!(
            "refusing to replace existing pin {} ({}) with {} without --force (non-TTY)",
            existing.fqn,
            existing.anchor_label(),
            anchor_label(incoming),
        )));
    }
    let prompt = format!(
        "Replace pin {} ({}, sha {}) with {}?",
        existing.fqn,
        existing.anchor_label(),
        existing.short_sha(),
        anchor_label(incoming),
    );
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(default_yes)
        .interact()
        .map_err(|e| pay_core::Error::Config(format!("prompt failed: {e}")))
}

fn anchor_label(anchor: &PinAnchor) -> String {
    match anchor {
        PinAnchor::Pr { pr, .. } => format!("PR {pr}"),
        PinAnchor::Branch { ref_name } => format!("branch {ref_name}"),
        PinAnchor::Sha => "specific sha".to_string(),
    }
}

fn short_sha(sha: &str) -> &str {
    if sha.len() >= 7 {
        &sha[..7]
    } else {
        sha
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_pr(pr: u32, sha: &str) -> PinManifest {
        PinManifest {
            fqn: "venice/ai".into(),
            source_repo: DEFAULT_REPO.into(),
            head_repo: DEFAULT_REPO.into(),
            anchor: PinAnchor::Pr {
                pr,
                head_ref: "feat/x".into(),
            },
            sha: sha.into(),
            installed_at: "2026-06-12T00:00:00Z".into(),
            merged: false,
            files: vec![],
        }
    }

    fn pin_branch(name: &str, sha: &str) -> PinManifest {
        PinManifest {
            anchor: PinAnchor::Branch {
                ref_name: name.into(),
            },
            sha: sha.into(),
            ..pin_pr(0, sha)
        }
    }

    #[test]
    fn conflict_same_pr_same_sha_is_no_op() {
        let existing = pin_pr(137, "abcdef1234");
        let incoming = PinAnchor::Pr {
            pr: 137,
            head_ref: "feat/x".into(),
        };
        assert!(matches!(
            classify_conflict(&existing, &incoming, "abcdef1234"),
            Conflict::SameAnchorSameSha
        ));
    }

    #[test]
    fn conflict_same_pr_moved_sha_refreshes() {
        let existing = pin_pr(137, "abcdef1234");
        let incoming = PinAnchor::Pr {
            pr: 137,
            head_ref: "feat/x".into(),
        };
        assert!(matches!(
            classify_conflict(&existing, &incoming, "fedcba9876"),
            Conflict::SameAnchorMovedSha
        ));
    }

    #[test]
    fn conflict_different_pr_prompts_with_default_yes() {
        let existing = pin_pr(89, "11111");
        let incoming = PinAnchor::Pr {
            pr: 137,
            head_ref: "feat/x".into(),
        };
        match classify_conflict(&existing, &incoming, "22222") {
            Conflict::CrossKindOrAnchor { default_yes } => assert!(default_yes),
            _ => panic!("expected CrossKindOrAnchor"),
        }
    }

    #[test]
    fn conflict_branch_to_pr_prompts_with_default_no() {
        let existing = pin_branch("experimental", "11111");
        let incoming = PinAnchor::Pr {
            pr: 137,
            head_ref: "feat/x".into(),
        };
        match classify_conflict(&existing, &incoming, "22222") {
            Conflict::CrossKindOrAnchor { default_yes } => assert!(!default_yes),
            _ => panic!("expected CrossKindOrAnchor"),
        }
    }

    #[test]
    fn conflict_same_branch_same_sha_is_no_op() {
        let existing = pin_branch("dev", "abcdef1");
        let incoming = PinAnchor::Branch {
            ref_name: "dev".into(),
        };
        assert!(matches!(
            classify_conflict(&existing, &incoming, "abcdef1"),
            Conflict::SameAnchorSameSha
        ));
    }

    #[test]
    fn conflict_different_branch_prompts_with_default_yes() {
        let existing = pin_branch("dev", "11111");
        let incoming = PinAnchor::Branch {
            ref_name: "experimental".into(),
        };
        match classify_conflict(&existing, &incoming, "22222") {
            Conflict::CrossKindOrAnchor { default_yes } => assert!(default_yes),
            _ => panic!("expected CrossKindOrAnchor"),
        }
    }
}
