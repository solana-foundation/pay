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
