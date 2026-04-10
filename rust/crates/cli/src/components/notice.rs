//! Styled notice box for CLI output.
//!
//! Renders an icon "pill" (background color + contrasting foreground)
//! followed by a bold title and a dimmed multi-line body:
//!
//! ```text
//!    ⚠   Title of the notice
//!       First line of the body.
//!       Second line of the body.
//! ```

use owo_colors::OwoColorize;

/// Severity of a notice — determines the icon and color pair.
#[derive(Debug, Clone, Copy)]
pub enum NoticeLevel {
    Info,
    Success,
    Warning,
    Error,
}

impl NoticeLevel {
    fn icon(&self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Success => "✓",
            Self::Warning => "⚠",
            Self::Error => "✗",
        }
    }

    /// Render the icon as a "pill" — single space of padding on each side,
    /// background color set to the level's color, foreground set to a
    /// high-contrast complement so the icon stays readable.
    fn pill(&self) -> String {
        let padded = format!(" {} ", self.icon());
        match self {
            Self::Info => padded.white().on_blue().bold().to_string(),
            Self::Success => padded.black().on_green().bold().to_string(),
            Self::Warning => padded.black().on_yellow().bold().to_string(),
            Self::Error => padded.white().on_red().bold().to_string(),
        }
    }
}

/// Render a notice with a title and multi-line body.
///
/// The icon is rendered as a colored pill; the title is bold; body lines
/// are dimmed and indented under the title.
pub fn notice(level: NoticeLevel, title: &str, body: &str) -> String {
    let pill = level.pill();
    let title = title.bold();
    let mut out = format!("\n  {pill}  {title}\n");
    for line in body.lines() {
        out.push_str(&format!("       {}\n", line.dimmed()));
    }
    out
}
