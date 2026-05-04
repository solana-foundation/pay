//! Probe helpers shared by `pay catalog build` modes (single-file, changed-from,
//! full registry). Loads provider specs from disk, resolves OpenAPI to an
//! endpoint list, and renders raw probe results when `--verbose` is on.

use std::path::PathBuf;

use owo_colors::OwoColorize;

use pay_core::skills::build::parse_frontmatter;
use pay_core::skills::probe::{ProbeReport, ProbeStatus};

/// Parse a single PAY.md / .md file into a `ProbeProvider`. The FQN is derived
/// via [`super::derive_fqn_from_path`] so single-segment FQNs (e.g. `syra/PAY.md`)
/// work the same way as in `pay catalog build`.
pub fn parse_single_provider(
    path: &std::path::Path,
) -> pay_core::Result<pay_types::registry::ProbeProvider> {
    let (fqn, _name, _op, _origin) = super::derive_fqn_from_path(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| pay_core::Error::Config(format!("read {}: {e}", path.display())))?;
    let (yaml_str, _) = parse_frontmatter(&text)?;
    let spec: pay_types::registry::ProviderFrontmatter = serde_yml::from_str(&yaml_str)
        .map_err(|e| pay_core::Error::Config(format!("{}: {e}", path.display())))?;

    let spec_dir = path.parent();
    let resolved = pay_core::skills::openapi::effective_endpoints_relative_to(&spec, spec_dir)?;
    let openapi_driven = spec.openapi.is_some();
    let endpoints = resolved
        .into_iter()
        .map(|r| pay_types::registry::ProbeEndpoint {
            method: r.spec.method,
            path: r.spec.path,
            metered: openapi_driven || r.spec.pricing.is_some(),
            body: r.body_example,
        })
        .collect();

    Ok(pay_types::registry::ProbeProvider {
        fqn,
        service_url: spec.meta.service_url,
        endpoints,
    })
}

/// Collect all providers from the registry directory.
pub fn collect_all_providers(
    root: &std::path::Path,
) -> pay_core::Result<Vec<pay_types::registry::ProbeProvider>> {
    let providers_dir = root.join("providers");
    if !providers_dir.is_dir() {
        return Err(pay_core::Error::Config(format!(
            "No providers/ directory at {}",
            root.display()
        )));
    }

    let mut result = Vec::new();
    walk_providers(&providers_dir, &providers_dir, &mut result)?;
    result.sort_by(|a, b| a.fqn.cmp(&b.fqn));
    Ok(result)
}

/// Walk the providers directory tree looking for `<dir>/PAY.md` files. The
/// first PAY.md encountered along a path stops the recursion: deeper
/// directories under that provider are not searched.
fn walk_providers(
    dir: &std::path::Path,
    providers_root: &std::path::Path,
    result: &mut Vec<pay_types::registry::ProbeProvider>,
) -> pay_core::Result<()> {
    let pay_md = dir.join("PAY.md");
    if pay_md.is_file() {
        if let Some(provider) = parse_provider_file(&pay_md, providers_root)? {
            result.push(provider);
        }
        return Ok(());
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            walk_providers(&path, providers_root, result)?;
        }
    }
    Ok(())
}

/// Collect specific providers from file paths.
pub fn collect_specific_providers(
    root: &std::path::Path,
    files: &[PathBuf],
) -> pay_core::Result<Vec<pay_types::registry::ProbeProvider>> {
    let providers_root = root.join("providers");
    let mut result = Vec::new();

    for file in files {
        let full_path = if file.is_absolute() {
            file.clone()
        } else {
            root.join(file)
        };
        if !full_path.exists() {
            eprintln!(
                "  {} skipping {}: file not found",
                "!".yellow(),
                file.display()
            );
            continue;
        }
        if let Some(provider) = parse_provider_file(&full_path, &providers_root)? {
            result.push(provider);
        }
    }

    Ok(result)
}

/// Parse a provider .md file into a `ProbeProvider`.
fn parse_provider_file(
    path: &std::path::Path,
    providers_root: &std::path::Path,
) -> pay_core::Result<Option<pay_types::registry::ProbeProvider>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| pay_core::Error::Config(format!("read {}: {e}", path.display())))?;
    let (yaml_str, _) = parse_frontmatter(&text)?;
    let spec: pay_types::registry::ProviderFrontmatter = serde_yml::from_str(&yaml_str)
        .map_err(|e| pay_core::Error::Config(format!("{}: {e}", path.display())))?;

    // Build FQN from path relative to providers/. PAY.md lives inside its
    // provider's directory, so the FQN is the parent directory's relative
    // path (e.g. `providers/quicknode/rpc/PAY.md` → `quicknode/rpc`).
    let provider_dir = path.parent().unwrap_or(path);
    let fqn = provider_dir
        .strip_prefix(providers_root)
        .unwrap_or(provider_dir)
        .to_string_lossy()
        .replace('\\', "/");

    // If the spec uses `openapi:` instead of inline `endpoints:`, resolve the
    // OpenAPI document into an endpoint list. Resolved endpoints carry no
    // pricing — flag them all `metered: true` so the prober actually hits
    // them and lets the 402 response classify each one. The OpenAPI resolver
    // also produces a `body_example` for POST/PUT/PATCH operations so probes
    // get past server-side schema validation before the paywall fires.
    let spec_dir = path.parent();
    let resolved = pay_core::skills::openapi::effective_endpoints_relative_to(&spec, spec_dir)?;
    let openapi_driven = spec.openapi.is_some();
    let endpoints = resolved
        .into_iter()
        .map(|r| pay_types::registry::ProbeEndpoint {
            method: r.spec.method,
            path: r.spec.path,
            metered: openapi_driven || r.spec.pricing.is_some(),
            body: r.body_example,
        })
        .collect();

    Ok(Some(pay_types::registry::ProbeProvider {
        fqn,
        service_url: spec.meta.service_url,
        endpoints,
    }))
}

/// Render results as a colored table.
pub fn render_probe_table(report: &ProbeReport) {
    for provider in &report.providers {
        let status_icon = if provider.pass {
            "OK".green().to_string()
        } else {
            "FAIL".red().to_string()
        };
        eprintln!("{} {}", provider.fqn.bold(), status_icon);

        for ep in &provider.endpoints {
            let (icon, detail) = match &ep.status {
                ProbeStatus::Ok {
                    protocol, currency, ..
                } => (
                    "OK".green().to_string(),
                    format!("402 {protocol:<12} {currency}"),
                ),
                ProbeStatus::Free => ("--".dimmed().to_string(), "free".dimmed().to_string()),
                ProbeStatus::WrongChain { details } => {
                    ("FAIL".red().to_string(), format!("wrong chain: {details}"))
                }
                ProbeStatus::WrongCurrency { got, accepted } => (
                    "FAIL".red().to_string(),
                    format!("currency {got} not in {}", accepted.join(",")),
                ),
                ProbeStatus::UnknownProtocol => {
                    ("FAIL".red().to_string(), "unknown 402 protocol".into())
                }
                ProbeStatus::NotPaywalled { status_code } => (
                    "FAIL".red().to_string(),
                    format!("expected 402, got {status_code}"),
                ),
                ProbeStatus::Error { message } => {
                    ("ERR".red().to_string(), format!("error: {message}"))
                }
            };

            eprintln!(
                "  {:<6} {:<50} {icon:<4} {detail} ({}ms)",
                ep.method.dimmed(),
                ep.path,
                ep.duration_ms,
            );
        }
        eprintln!();
    }
}

/// Probe provider details into a [`ProbeReport`] using the standard
/// timeout/concurrency knobs. Thin wrapper used by `pay catalog build` to keep
/// the call site short.
pub fn run_probe(
    providers: Vec<pay_types::registry::ProbeProvider>,
    config: &pay_core::skills::probe::ProbeConfig,
) -> ProbeReport {
    pay_core::skills::probe::probe_providers(providers, config)
}
