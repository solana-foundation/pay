pub mod build;
pub mod check;
pub mod probe;
pub mod scaffold;
pub mod verdict;

use std::path::Path;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum CatalogCommand {
    /// Scaffold a new provider PAY.md from an OpenAPI document URL.
    Scaffold(scaffold::ScaffoldCommand),
    /// Read-only validation: parse + frontmatter check + probe + Solana verdict.
    /// Single-file, `--changed-from <REF>`, or full registry. Used by PR CI
    /// and local devex; never writes to disk.
    Check(check::CheckCommand),
    /// Full registry build. Runs the same checks as `check`, then writes
    /// `dist/skills.json` + per-provider detail files. Used by main-branch
    /// CI on a green tree.
    Build(build::BuildCommand),
}

impl CatalogCommand {
    pub fn run(self) -> pay_core::Result<()> {
        match self {
            Self::Scaffold(cmd) => cmd.run(),
            Self::Check(cmd) => cmd.run(),
            Self::Build(cmd) => cmd.run(),
        }
    }
}

/// Derive `(fqn, name, operator, origin)` from a provider file path.
///
/// Conventions:
/// - `…/<op>/<name>/PAY.md`              → fqn `<op>/<name>`,        op/origin/name = `op/op/name`
/// - `…/<op>/<origin>/<name>/PAY.md`     → fqn `<op>/<origin>/<name>`, op/origin/name as labelled
/// - `…/<op>/<name>.md` (legacy)         → fqn `<op>/<name>`,        op/origin/name = `op/op/name`
/// - `…/<op>/<origin>/<name>.md` (legacy)→ fqn `<op>/<origin>/<name>`, op/origin/name as labelled
/// - `<name>/PAY.md` (single-segment)    → fqn `<name>`,             op/origin/name = `name/name/name`
///
/// The path is taken relative to the current working directory so an absolute
/// canonicalized path doesn't bleed into the FQN.
pub fn derive_fqn_from_path(path: &Path) -> pay_core::Result<(String, String, String, String)> {
    let cwd =
        std::env::current_dir().map_err(|e| pay_core::Error::Config(format!("get cwd: {e}")))?;
    let rel = path.strip_prefix(&cwd).unwrap_or(path);

    let basename = rel
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| pay_core::Error::Config("file has no name".into()))?;
    let parent = rel.parent().ok_or_else(|| {
        pay_core::Error::Config(format!("{} has no parent directory", rel.display()))
    })?;

    let mut segments: Vec<String> = parent
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(String::from))
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .collect();

    if basename != "PAY.md" {
        let stem = std::path::Path::new(basename)
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| pay_core::Error::Config("file has no stem".into()))?;
        segments.push(stem.to_string());
    }

    if segments.is_empty() {
        return Err(pay_core::Error::Config(format!(
            "cannot derive FQN from `{}` — file has no parent directory to use as a name",
            path.display()
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
