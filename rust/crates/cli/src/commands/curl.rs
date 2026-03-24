use clap::Args;

/// Make an HTTP request, handling 402 Payment Required flows.
///
/// All arguments are passed through to the real curl binary.
#[derive(Args)]
pub struct CurlCommand {
    /// Arguments forwarded to curl.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}
