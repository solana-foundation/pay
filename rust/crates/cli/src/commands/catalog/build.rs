//! `pay catalog build` — full-registry build that writes `dist/skills.json`
//! and per-provider detail files. Use `pay catalog check` for a read-only
//! validation pass that does not touch disk (PR-CI flow).

use std::fs;
use std::path::{Path, PathBuf};

use owo_colors::OwoColorize;

use super::check::print_validation_errors;
use crate::components::{NoticeLevel, print_notice};

#[derive(clap::Args)]
pub struct BuildCommand {
    /// Path to the pay-skills registry directory (containing `providers/`,
    /// `affiliates/`, `aggregators/`).
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// CDN base URL for detail file references in the index.
    #[arg(long, default_value = "https://storage.googleapis.com/pay-skills/v1")]
    pub base_url: String,

    /// Output directory (default: <path>/dist).
    #[arg(long, short)]
    pub output: Option<PathBuf>,

    /// Skip live probing of endpoints (faster, but no probe-derived
    /// pricing/protocol/supported_usd metadata in the output).
    #[arg(long)]
    pub no_probe: bool,

    /// Per-endpoint probe timeout in seconds.
    #[arg(long, default_value_t = 10)]
    pub probe_timeout: u64,

    /// Max concurrent provider probes.
    #[arg(long, default_value_t = 5)]
    pub probe_concurrency: usize,

    /// Comma-separated FQNs to (re)build from source. Every other provider
    /// is copied verbatim from `--previous-dist`. Useful for fast partial
    /// rebuilds at merge time.
    #[arg(long, value_delimiter = ',', value_name = "FQN1,FQN2,...")]
    pub only: Vec<String>,

    /// Path to a previously-built `dist/` directory. Required when `--only`
    /// is set; unchanged providers are sourced from here.
    #[arg(long, value_name = "DIR")]
    pub previous_dist: Option<PathBuf>,
}

impl BuildCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let root = self.path.canonicalize().map_err(|e| {
            pay_core::Error::Config(format!("invalid path `{}`: {e}", self.path.display()))
        })?;
        if !root.is_dir() {
            return Err(pay_core::Error::Config(format!(
                "{} is not a directory; for single-file validation use `pay catalog check <path>`",
                root.display()
            )));
        }

        let dist = self.output.clone().unwrap_or_else(|| root.join("dist"));

        eprintln!(
            "Building skills index from {}",
            root.display().to_string().bold()
        );

        let now = format_utc_timestamp(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );

        let only = if self.only.is_empty() {
            None
        } else {
            Some(self.only.iter().cloned().collect())
        };
        let options = pay_core::skills::build::BuildOptions {
            probe: !self.no_probe,
            probe_config: pay_core::skills::probe::ProbeConfig {
                timeout_secs: self.probe_timeout,
                concurrency: self.probe_concurrency,
                ..Default::default()
            },
            only,
            previous_dist: self.previous_dist.clone(),
        };
        let result =
            pay_core::skills::build::build_with_options(&root, &self.base_url, now, &options);

        if !result.errors.is_empty() {
            print_validation_errors(&result.errors);
            // Refuse to write a partial dist when validation fails — would
            // mislead consumers into thinking those providers shipped clean.
            std::process::exit(1);
        }

        write_dist(&dist, &result)?;

        let body = format!(
            "{} ({} providers, {} affiliates, {} aggregators)\n\
             {} provider detail files in {}/",
            dist.join("skills.json").display(),
            result.index.provider_count,
            result.index.affiliate_count,
            result.index.aggregator_count,
            result.detail_files.len(),
            dist.join("providers").display(),
        );
        print_notice(NoticeLevel::Success, "Build complete", &body);
        Ok(())
    }
}

fn write_dist(dist: &Path, result: &pay_core::skills::build::BuildResult) -> pay_core::Result<()> {
    if dist.exists() {
        fs::remove_dir_all(dist).map_err(|e| {
            pay_core::Error::Config(format!("failed to clean {}: {e}", dist.display()))
        })?;
    }
    for (rel_path, json) in &result.detail_files {
        let full_path = dist.join(rel_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| pay_core::Error::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        fs::write(&full_path, json)
            .map_err(|e| pay_core::Error::Config(format!("write {}: {e}", full_path.display())))?;
    }
    let index_json = serde_json::to_string_pretty(&result.index)
        .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
    let index_path = dist.join("skills.json");
    fs::create_dir_all(dist)
        .map_err(|e| pay_core::Error::Config(format!("mkdir {}: {e}", dist.display())))?;
    fs::write(&index_path, format!("{index_json}\n"))
        .map_err(|e| pay_core::Error::Config(format!("write {}: {e}", index_path.display())))?;
    Ok(())
}

/// Format unix epoch seconds as ISO 8601 UTC timestamp.
fn format_utc_timestamp(epoch_secs: u64) -> String {
    // Civil date from epoch days (algorithm from Howard Hinnant).
    let days = (epoch_secs / 86400) as i64;
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let time_of_day = epoch_secs % 86400;
    let h = time_of_day / 3600;
    let min = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;

    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z")
}
