use serde::{Deserialize, Serialize};

/// Output format for CLI status messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Human-readable text output.
    #[default]
    Text,
    /// Machine-readable JSON output.
    Json,
}

/// Print a JSON value to stdout (compact when NO_DNA, pretty for humans).
pub fn print_json(value: &serde_json::Value) -> pay_core::Result<()> {
    let s = if crate::no_dna::is_agent() {
        serde_json::to_string(value)?
    } else {
        serde_json::to_string_pretty(value)?
    };
    println!("{s}");
    Ok(())
}

/// Write a structured error to stderr as JSON.
pub fn error_json(message: &str) {
    let json = serde_json::json!({
        "error": {
            "message": message,
        }
    });
    if let Ok(s) = serde_json::to_string(&json) {
        eprintln!("{s}");
    }
}
