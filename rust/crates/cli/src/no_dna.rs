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
