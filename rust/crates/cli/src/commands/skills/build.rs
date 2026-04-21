use std::fs;
use std::path::PathBuf;

use owo_colors::OwoColorize;

/// Build the skills index from a pay-skills registry directory.
///
/// Reads `.md` files from `providers/`, `affiliates/`, `aggregators/`
/// and produces `dist/skills.json` + per-provider detail files.
#[derive(clap::Args)]
pub struct BuildCommand {
    /// Path to the pay-skills registry directory (containing providers/, affiliates/, aggregators/).
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// CDN base URL for detail file references in the index.
    #[arg(long, default_value = "https://storage.googleapis.com/pay-skills-cdn/v2")]
    pub base_url: String,

    /// Output directory (default: <path>/dist).
    #[arg(long, short)]
    pub output: Option<PathBuf>,
}

impl BuildCommand {
    pub fn run(self) -> pay_core::Result<()> {
        let root = self.path.canonicalize().map_err(|e| {
            pay_core::Error::Config(format!("invalid path `{}`: {e}", self.path.display()))
        })?;

        let dist = self
            .output
            .unwrap_or_else(|| root.join("dist"));

        eprintln!(
            "Building skills index from {}",
            root.display().to_string().bold()
        );
        eprintln!();

        // Generate timestamp (chrono is in the CLI crate's dep tree via other paths).
        let now = {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            // Use the unix timestamp — detail formatting happens in core.
            // For now, just pass epoch seconds and let core format it.
            // Actually, let's format it here since CLI owns the I/O boundary.
            format_utc_timestamp(d.as_secs())
        };

        let result = pay_core::skills::build::build(&root, &self.base_url, now);

        // Report errors
        if !result.errors.is_empty() {
            eprintln!("{}", "Validation errors:".red().bold());
            for err in &result.errors {
                eprintln!("  {} {err}", "-".red());
            }
            eprintln!();
        }

        // Write dist/
        if dist.exists() {
            fs::remove_dir_all(&dist).map_err(|e| {
                pay_core::Error::Config(format!("failed to clean {}: {e}", dist.display()))
            })?;
        }

        // Write per-provider detail files
        for (rel_path, json) in &result.detail_files {
            let full_path = dist.join(rel_path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    pay_core::Error::Config(format!("mkdir {}: {e}", parent.display()))
                })?;
            }
            fs::write(&full_path, json).map_err(|e| {
                pay_core::Error::Config(format!("write {}: {e}", full_path.display()))
            })?;
        }

        // Write index
        let index_json = serde_json::to_string_pretty(&result.index)
            .map_err(|e| pay_core::Error::Config(format!("json: {e}")))?;
        let index_path = dist.join("skills.json");
        fs::create_dir_all(&dist).map_err(|e| {
            pay_core::Error::Config(format!("mkdir {}: {e}", dist.display()))
        })?;
        fs::write(&index_path, format!("{index_json}\n")).map_err(|e| {
            pay_core::Error::Config(format!("write {}: {e}", index_path.display()))
        })?;

        // Summary
        eprintln!(
            "Wrote {} ({} providers, {} affiliates, {} aggregators)",
            index_path.display().to_string().bold(),
            result.index.provider_count.to_string().green(),
            result.index.affiliate_count.to_string().green(),
            result.index.aggregator_count.to_string().green(),
        );
        eprintln!(
            "Wrote {} provider detail files to {}/",
            result.detail_files.len().to_string().green(),
            dist.join("providers").display().to_string().bold(),
        );

        if !result.errors.is_empty() {
            eprintln!();
            eprintln!(
                "{}",
                format!("{} error(s) found", result.errors.len()).red().bold()
            );
            std::process::exit(1);
        }

        Ok(())
    }
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
