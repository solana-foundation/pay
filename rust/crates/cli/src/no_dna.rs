use crate::output::OutputFormat;

/// Returns `true` when the caller is a non-human operator (AI agent, automation).
///
/// Detection follows the NO_DNA standard (<https://no-dna.org>):
/// the `NO_DNA` environment variable is set and non-empty.
pub fn is_agent() -> bool {
    std::env::var("NO_DNA").is_ok_and(|v| !v.is_empty())
}

/// Resolve whether output should be JSON.
///
/// Precedence (highest to lowest):
/// 1. Explicit `--output` flag (if user passed it)
/// 2. `NO_DNA` env var -> JSON
/// 3. TTY detection -> text for terminals, JSON for pipes
pub fn should_json(explicit_output: Option<OutputFormat>) -> bool {
    if let Some(fmt) = explicit_output {
        return fmt == OutputFormat::Json;
    }

    if is_agent() {
        return true;
    }

    !std::io::IsTerminal::is_terminal(&std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputFormat;

    #[test]
    fn should_json_explicit_json() {
        assert!(should_json(Some(OutputFormat::Json)));
    }

    #[test]
    fn should_json_explicit_text() {
        assert!(!should_json(Some(OutputFormat::Text)));
    }

    // Note: is_agent() relies on env vars which can't be safely tested
    // in parallel. We test the should_json logic with explicit overrides
    // and verify is_agent's logic by reading the code.

    #[test]
    fn should_json_none_in_pipe() {
        // When run in tests, stdout is not a terminal, so should_json(None)
        // depends on is_agent() and TTY detection. With NO_DNA unset and
        // stdout piped (as in test runner), this returns true (piped → JSON).
        // This is non-deterministic, so just verify it doesn't panic.
        let _ = should_json(None);
    }
}
