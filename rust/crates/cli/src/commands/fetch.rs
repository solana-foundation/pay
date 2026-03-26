use clap::Args;

/// Fetch a URL using the built-in HTTP client (no external tool required).
///
/// Prints the response body to stdout. Handles 402 Payment Required flows.
#[derive(Args)]
pub struct FetchCommand {
    /// The URL to fetch.
    pub url: String,

    /// Extra headers in "Key: Value" format.
    #[arg(short = 'H', long = "header")]
    pub headers: Vec<String>,
}
