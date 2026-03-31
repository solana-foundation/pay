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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_format_default_is_text() {
        assert_eq!(OutputFormat::default(), OutputFormat::Text);
    }

    #[test]
    fn output_format_serde_roundtrip() {
        for fmt in [OutputFormat::Text, OutputFormat::Json] {
            let json = serde_json::to_string(&fmt).unwrap();
            let back: OutputFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(back, fmt);
        }
    }

    #[test]
    fn print_json_does_not_panic() {
        let value = serde_json::json!({"test": true});
        // This writes to stdout; just verify it doesn't panic
        let _ = print_json(&value);
    }

    #[test]
    fn error_json_does_not_panic() {
        // This writes to stderr; just verify it doesn't panic
        error_json("test error");
    }
}
