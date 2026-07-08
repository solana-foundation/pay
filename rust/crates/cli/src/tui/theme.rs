//! Shared color palette + border conventions for the pay TUIs.
//!
//! Border conventions used across flows: rounded borders
//! ([`ratatui::widgets::BorderType::Rounded`]) everywhere, green border for
//! the focused control, dark-gray for unfocused ones.

use ratatui::style::Color;

/// Background of the session card column.
pub(crate) const CARD_BG: Color = Color::Rgb(35, 40, 50);
/// Dark sidebar background shared by the topup and session layouts.
pub(crate) const TOPUP_SIDEBAR_BG: Color = Color::Rgb(24, 24, 27);
/// Near-black main-content background.
pub(crate) const TOPUP_MAIN_BG: Color = Color::Rgb(9, 9, 11);
/// Background of unselected option cards / buttons.
pub(crate) const TOPUP_CARD_BG: Color = Color::Rgb(39, 39, 42);
pub(crate) const SOLANA_PURPLE: Color = Color::Rgb(153, 69, 255);
pub(crate) const SOLANA_BLUE: Color = Color::Rgb(80, 120, 255);
pub(crate) const SOLANA_GREEN: Color = Color::Rgb(20, 241, 149);

/// Border color of the session card.
pub(crate) const CARD_BORDER: Color = Color::Rgb(60, 65, 75);
/// Face (fill) color of the session card.
pub(crate) const CARD_FACE: Color = Color::Rgb(35, 40, 50);
