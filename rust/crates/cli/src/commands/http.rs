use clap::Args;

/// Make an HTTP request via HTTPie, handling 402 Payment Required flows.
///
/// All arguments are passed through to the real `http` binary.
#[derive(Args)]
#[command(disable_help_flag = true)]
pub struct HttpCommand {
    /// Arguments forwarded to httpie.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}
