//! Terminal hyperlink helpers (OSC 8 escape sequences).
//!
//! Used to print clickable links in the terminal. Supported by iTerm2,
//! GNOME Terminal, kitty, WezTerm, and most modern terminal emulators.

use owo_colors::OwoColorize;

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
pub fn solana_explorer_cluster_query(network: &str, rpc_url: &str) -> String {
    match network {
        "mainnet" | "mainnet-beta" => String::new(),
        "devnet" => "?cluster=devnet".to_string(),
        "localnet" | "sandbox" => {
            format!("?cluster=custom&customUrl={}", urlencoding::encode(rpc_url))
        }
        _ => String::new(),
    }
}

/// Link to a Solana transaction receipt on Solana Explorer.
pub fn solana_transaction_link(signature: &str, _network: &str, _rpc_url: &str) -> String {
    let url =
        format!("https://explorer.solana.com/tx/{signature}?cluster=mainnet-beta&view=receipt");
    link_with_arrow("Link to receipt", &url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solana_transaction_link_uses_mainnet_receipt_view() {
        let rendered = solana_transaction_link("sig123", "mainnet", "");

        assert!(
            rendered.contains(
                "https://explorer.solana.com/tx/sig123?cluster=mainnet-beta&view=receipt"
            )
        );
        assert!(rendered.contains("Link to receipt"));
    }
}
