//! Built-in HTTP client using reqwest. No external binary needed.

use reqwest::blocking::Client;
use tracing::debug;

use crate::runner::{self, RunOutcome};
use crate::{Error, Result};

/// Fetch a URL, detecting 402 + MPP challenges.
pub fn fetch(url: &str, extra_headers: &[(String, String)]) -> Result<RunOutcome> {
    let client = Client::builder()
        .user_agent(format!("pay/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Mpp(format!("Failed to create HTTP client: {e}")))?;

    debug!(%url, "Fetching");

    let mut req = client.get(url);
    for (key, value) in extra_headers {
        req = req.header(key.as_str(), value.as_str());
    }

    let resp = req
        .send()
        .map_err(|e| Error::Mpp(format!("Request failed: {e}")))?;
    let status = resp.status().as_u16();

    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_lowercase(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();

    let body = resp
        .text()
        .map_err(|e| Error::Mpp(format!("Failed to read body: {e}")))?;

    debug!(status, "Fetch complete");

    if status == 402 {
        return Ok(runner::classify_402(&headers, Some(&body), url));
    }

    let exit_code = if status >= 400 { 1 } else { 0 };
    Ok(RunOutcome::Completed {
        exit_code,
        body: Some(body),
    })
}
