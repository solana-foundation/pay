use std::path::PathBuf;

use clap::ValueEnum;
use owo_colors::OwoColorize;

use pay_core::skills::build::parse_frontmatter;
use pay_core::skills::probe::{ProbeConfig, ProbeReport, ProbeStatus};

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

/// Probe provider endpoints to verify they return valid Solana 402 challenges.
#[derive(clap::Args)]
pub struct ProbeCommand {
    /// Path to the pay-skills registry directory.
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Specific provider .md files to probe (relative to path).
    /// When omitted, probes all providers.
    #[arg(long)]
    pub files: Vec<PathBuf>,

    /// Accepted stablecoin symbols (comma-separated).
    #[arg(long, default_value = "USDC,USDT", value_delimiter = ',')]
    pub currencies: Vec<String>,

    /// Per-endpoint timeout in seconds.
    #[arg(long, default_value = "10")]
    pub timeout: u64,

    /// Max concurrent provider probes.
    #[arg(long, default_value = "5")]
    pub concurrency: usize,

    /// Output format.
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

impl ProbeCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let config = ProbeConfig {
            accepted_currencies: self.currencies.iter().map(|c| c.to_uppercase()).collect(),
            timeout_secs: self.timeout,
            concurrency: self.concurrency,
        };

        let providers = if self.files.is_empty() {
            collect_all_providers(&self.path)?
        } else {
            collect_specific_providers(&self.path, &self.files)?
        };

        if providers.is_empty() {
            eprintln!("{}", "No provider files found.".yellow());
            return Ok(());
        }

        let total_eps: usize = providers.iter().map(|p| p.endpoints.len()).sum();
        eprintln!(
            "Probing {} provider(s) ({} endpoints)...",
            providers.len().to_string().bold(),
            total_eps.to_string().bold(),
        );
        eprintln!();

        let report = pay_core::skills::probe::probe_providers(providers, &config);

        match self.format {
            OutputFormat::Table => render_table(&report),
            OutputFormat::Json => render_json(&report)?,
        }

        if report.failed > 0 {
            std::process::exit(1);
        }

        Ok(())
    }
}

/// Collect all providers from the registry directory.
fn collect_all_providers(
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

/// Walk the providers directory tree and collect provider specs.
fn walk_providers(
    dir: &std::path::Path,
    providers_root: &std::path::Path,
    result: &mut Vec<pay_types::registry::ProbeProvider>,
) -> pay_core::Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            walk_providers(&path, providers_root, result)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md")
            && let Some(provider) = parse_provider_file(&path, providers_root)?
        {
            result.push(provider);
        }
    }
    Ok(())
}

/// Collect specific providers from file paths.
fn collect_specific_providers(
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

    // Build FQN from path relative to providers/
    let fqn = path
        .strip_prefix(providers_root)
        .unwrap_or(path)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");

    let endpoints = spec
        .endpoints
        .iter()
        .map(|ep| pay_types::registry::ProbeEndpoint {
            method: ep.method.clone(),
            path: ep.path.clone(),
            metered: ep.pricing.is_some(),
        })
        .collect();

    Ok(Some(pay_types::registry::ProbeProvider {
        fqn,
        service_url: spec.meta.service_url,
        endpoints,
    }))
}

/// Render results as a colored table.
fn render_table(report: &ProbeReport) {
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

    let summary = format!(
        "Results: {}/{} endpoints passed",
        report.passed, report.total_endpoints
    );
    if report.failed == 0 {
        eprintln!("{}", summary.green());
    } else {
        eprintln!("{}", summary.red());
    }
}

/// Render results as JSON (for CI consumption).
fn render_json(report: &ProbeReport) -> pay_core::Result<()> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| pay_core::Error::Config(format!("JSON serialization: {e}")))?;
    println!("{json}");
    Ok(())
}
