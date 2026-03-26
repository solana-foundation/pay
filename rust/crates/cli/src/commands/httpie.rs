use clap::Args;

/// Make an HTTP request via httpie, handling 402 Payment Required flows.
///
/// All arguments are passed through to the real `http` binary.
#[derive(Args)]
pub struct HttpieCommand {
    /// Arguments forwarded to httpie.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}
