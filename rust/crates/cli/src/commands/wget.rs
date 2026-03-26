use clap::Args;

/// Download a resource, handling 402 Payment Required flows.
///
/// All arguments are passed through to the real wget binary.
#[derive(Args)]
pub struct WgetCommand {
    /// Arguments forwarded to wget.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}
