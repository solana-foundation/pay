//! Terminal hyperlink helpers (OSC 8 escape sequences).
//!
//! Used to print clickable links in the terminal. Supported by iTerm2,
//! GNOME Terminal, kitty, WezTerm, and most modern terminal emulators.

use owo_colors::OwoColorize;

use crate::network::SolanaExplorerCluster;

/// The character appended to visually indicate a clickable link.
pub const LINK_ARROW: &str = "↗";

/// Wrap `text` in an OSC 8 hyperlink pointing to `url`.
///
/// Note: the hyperlink covers only the exact `text` — no padding, no arrow.
/// Pairs well with [`link_with_arrow`] when you want a visible indicator.
pub fn link(text: &str, url: &str) -> String {
    format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, text)
}

/// Wrap `text` in an OSC 8 hyperlink and append a dimmed `↗` arrow after it.
///
/// The link only covers `text`, not the arrow, so the visible indicator
/// is outside the clickable area (avoiding extra padding being clickable).
pub fn link_with_arrow(text: &str, url: &str) -> String {
    format!("{} {}", link(text, url), LINK_ARROW.dimmed())
}

/// Build the `?cluster=...` query suffix for Solana Explorer URLs.
pub fn solana_explorer_cluster_query(cluster: &SolanaExplorerCluster) -> String {
    cluster.query_suffix()
}

/// Link to a Solana transaction receipt on Solana Explorer.
pub fn solana_transaction_link(signature: &str, cluster: &SolanaExplorerCluster) -> String {
    let url = format!(
        "https://explorer.solana.com/tx/{signature}{}",
        cluster.transaction_receipt_query_suffix()
    );
    link_with_arrow("Link to receipt", &url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solana_transaction_link_uses_mainnet_receipt_view() {
        let rendered = solana_transaction_link("sig123", &SolanaExplorerCluster::Mainnet);

        assert!(
            rendered.contains(
                "https://explorer.solana.com/tx/sig123?cluster=mainnet-beta&view=receipt"
            )
        );
        assert!(rendered.contains("Link to receipt"));
    }

    #[test]
    fn solana_transaction_link_uses_custom_rpc_url() {
        let rendered = solana_transaction_link(
            "sig123",
            &SolanaExplorerCluster::Custom {
                rpc_url: "http://localhost:8899".to_string(),
            },
        );

        assert!(rendered.contains(
            "https://explorer.solana.com/tx/sig123?cluster=custom&customUrl=http%3A%2F%2Flocalhost%3A8899&view=receipt"
        ));
    }
}
