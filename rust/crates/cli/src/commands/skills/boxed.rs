//! Shared 80-column bordered-box renderer for `pay skills` text output.
//!
//! `pay skills search` and `pay skills ls` frame their results in the same
//! box: a 78-wide border (`┌─┐ │ │ └─┘`) indented 2 spaces (80 columns max),
//! with `├───┤` rules dividing each section. Callers pre-render every line —
//! applying their own colors — and pass `(content, visible_width)` pairs so
//! the right gutter still lines up underneath the ANSI styling.

use owo_colors::OwoColorize;

/// Inner content width. Box width = `INNER + 4` (`"│ "` + content + `" │"`);
/// with the 2-space indent the rendered box caps at 80 columns.
pub(crate) const INNER: usize = 74;

/// Frame `sections` into a single bordered box. Each section is a list of
/// pre-rendered `(content, visible_width)` lines; sections are divided by a
/// `├───┤` rule. The whole box is indented 2 spaces to sit under a header.
pub(crate) fn frame(sections: &[Vec<(String, usize)>]) -> String {
    let bar = "│".dimmed().to_string();
    let rule = |left: char, right: char| {
        format!("{left}{}{right}", "─".repeat(INNER + 2))
            .dimmed()
            .to_string()
    };
    let mut out: Vec<String> = vec![rule('┌', '┐')];
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            out.push(rule('├', '┤'));
        }
        for (content, visible) in section {
            let pad = " ".repeat(INNER.saturating_sub((*visible).min(INNER)));
            out.push(format!("{bar} {content}{pad} {bar}"));
        }
    }
    out.push(rule('└', '┘'));
    indent(&out.join("\n"), 2)
}

/// Greedy word-wrap to `width` columns (by char count). Over-long tokens are
/// hard-split rather than truncated, so no content is ever lost.
pub(crate) fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if word.chars().count() > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let chars: Vec<char> = word.chars().collect();
            let mut idx = 0;
            while chars.len() - idx > width {
                lines.push(chars[idx..idx + width].iter().collect());
                idx += width;
            }
            cur = chars[idx..].iter().collect();
            continue;
        }
        let clen = cur.chars().count();
        let need = if clen == 0 {
            word.chars().count()
        } else {
            clen + 1 + word.chars().count()
        };
        if need > width {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Color `label` by provider `category` — a stable one-color-per-category map
/// so the same category always reads the same hue across the listing. Unknown
/// categories fall back to white. Returns the styled string; its display width
/// is unchanged (`label.chars().count()`).
pub(crate) fn category_color(category: &str, label: &str) -> String {
    match category {
        "ai_ml" => label.magenta().to_string(),
        "data" => label.blue().to_string(),
        "finance" => label.green().to_string(),
        "maps" => label.cyan().to_string(),
        "media" => label.red().to_string(),
        "messaging" => label.yellow().to_string(),
        "search" => label.bright_blue().to_string(),
        "translation" => label.bright_green().to_string(),
        "shopping" => label.bright_yellow().to_string(),
        "security" => label.bright_red().to_string(),
        "compute" => label.bright_cyan().to_string(),
        "storage" => label.bright_magenta().to_string(),
        "identity" => label.bright_white().to_string(),
        "productivity" => label.bright_magenta().to_string(),
        "cloud" => label.bright_cyan().to_string(),
        "devtools" => label.bright_white().to_string(),
        _ => label.white().to_string(),
    }
}

/// Indent every line by `n` spaces.
pub(crate) fn indent(s: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    s.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
