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

// ── Endpoint table ───────────────────────────────────────────────────────────

/// A single endpoint row for [`render_endpoint_table`]. Borrows its fields so
/// both `pay skills search` (from a `SearchHit`) and `pay skills show`
/// (from a catalog `Endpoint`) render an identical table.
pub(crate) struct EndpointRow<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub description: &'a str,
    pub pricing: Option<&'a serde_json::Value>,
    pub metered: bool,
}

/// Fixed display width of the method cell on a section's first line.
const METHOD_CELL: usize = 6;

/// Render endpoints as a single-column bordered box — no columns. Each endpoint
/// is its own section (divided by `├───┤` rules); inside a section the fields
/// stack on their own lines: `METHOD  price`, then the full path (bold), then
/// the description (dimmed). Long lines wrap (never truncate) to the inner
/// width.
pub(crate) fn render_endpoint_table(rows: &[EndpointRow]) -> String {
    let sections: Vec<Vec<(String, usize)>> = rows
        .iter()
        .map(|row| {
            let mut lines: Vec<(String, usize)> = Vec::new();
            // Line 1: colored method (6-col) + price. Color from the formatted
            // string, not `metered`: a metered endpoint with a $0 tier renders
            // as "free", which should read dim — not green.
            let price = format_price(row.pricing, row.metered);
            let price_visible = price.chars().count();
            let price = if price.starts_with('$') {
                price.green().to_string()
            } else {
                price.dimmed().to_string()
            };
            lines.push((
                format!("{}  {price}", color_method(row.method)),
                METHOD_CELL + 2 + price_visible,
            ));
            // Full endpoint path (bold), wrapped.
            for seg in wrap(row.path, INNER) {
                let visible = seg.chars().count();
                lines.push((seg.bold().to_string(), visible));
            }
            // Description (dimmed), wrapped.
            if !row.description.is_empty() {
                for seg in wrap(row.description, INNER) {
                    let visible = seg.chars().count();
                    lines.push((seg.dimmed().to_string(), visible));
                }
            }
            lines
        })
        .collect();
    frame(&sections)
}

/// Color + left-pad an HTTP method to [`METHOD_CELL`] columns. Pad before
/// coloring so the ANSI codes don't count toward the cell's display width.
fn color_method(method: &str) -> String {
    format!("{method:<width$}", width = METHOD_CELL)
        .cyan()
        .to_string()
}

/// Compact price string from the endpoint's `pricing` JSON. Handles the public
/// catalog `dimensions` shape, the local `{"usd"}` flat shape, and
/// `{"subscription"}` gating; falls back to `metered` / `free`.
pub(crate) fn format_price(pricing: Option<&serde_json::Value>, metered: bool) -> String {
    let Some(p) = pricing else {
        return if metered {
            "metered".into()
        } else {
            "free".into()
        };
    };
    if let Some(sub) = p.get("subscription") {
        let price = sub.get("price_usd").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let period = sub
            .get("period")
            .and_then(|v| v.as_str())
            .unwrap_or("period");
        return format!("{} / {period}", fmt_usd(price));
    }
    if let Some(dims) = p.get("dimensions").and_then(|d| d.as_array()) {
        let parts: Vec<String> = dims.iter().filter_map(format_dimension).collect();
        if !parts.is_empty() {
            return parts.join("  +  ");
        }
    }
    if let Some(usd) = p.get("usd").and_then(|v| v.as_f64()) {
        return if usd <= 0.0 {
            "free".into()
        } else {
            fmt_usd(usd)
        };
    }
    if metered {
        "metered".into()
    } else {
        "free".into()
    }
}

/// One metering dimension → e.g. `$0.001 / req`, `$5 in / 1M tok`, `$1–2 / req`.
fn format_dimension(d: &serde_json::Value) -> Option<String> {
    let unit = d.get("unit").and_then(|v| v.as_str()).unwrap_or("unit");
    let scale = d.get("scale").and_then(|v| v.as_u64()).unwrap_or(1);
    let direction = d.get("direction").and_then(|v| v.as_str());
    let tiers = d.get("tiers").and_then(|v| v.as_array())?;
    let prices: Vec<f64> = tiers
        .iter()
        .filter_map(|t| t.get("price_usd").and_then(|v| v.as_f64()))
        .collect();
    if prices.is_empty() {
        return None;
    }
    let dir = match direction {
        Some("input") => " in",
        Some("output") => " out",
        _ => "",
    };
    let label = unit_label(unit, scale);
    let min = prices.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if (max - min).abs() < f64::EPSILON {
        Some(format!("{}{dir} / {label}", fmt_usd(min)))
    } else {
        Some(format!("{}–{}{dir} / {label}", fmt_usd(min), fmt_usd(max)))
    }
}

/// Human unit label: `scale` is the number of `unit`s one `price_usd` covers.
fn unit_label(unit: &str, scale: u64) -> String {
    let u = match unit {
        "requests" => "req",
        "tokens" => "tok",
        "characters" => "char",
        "minutes" => "min",
        "pages" => "page",
        "images" => "image",
        other => other,
    };
    let prefix = match scale {
        1 => String::new(),
        1_000 => "1K ".to_string(),
        1_000_000 => "1M ".to_string(),
        1_000_000_000 => "1B ".to_string(),
        n => format!("{n} "),
    };
    format!("{prefix}{u}")
}

/// Format a USD amount compactly: `$0.001`, `$1.50`, `$5`, `$0`.
fn fmt_usd(n: f64) -> String {
    if n <= 0.0 {
        return "$0".to_string();
    }
    // Three decimals for sub-dollar amounts so prices like $0.015 don't round
    // to $0.02; full six only for micro-cent prices below $0.01.
    let s = if n >= 1.0 {
        format!("{n:.2}")
    } else if n >= 0.01 {
        format!("{n:.3}")
    } else {
        format!("{n:.6}")
    };
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("${s}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn fmt_usd_compact() {
        assert_eq!(fmt_usd(0.001), "$0.001");
        assert_eq!(fmt_usd(1.5), "$1.5");
        assert_eq!(fmt_usd(5.0), "$5");
        assert_eq!(fmt_usd(9.99), "$9.99");
        assert_eq!(fmt_usd(0.0), "$0");
        // Sub-dollar amounts keep 3 decimals instead of rounding to 2.
        assert_eq!(fmt_usd(0.015), "$0.015");
        assert_eq!(fmt_usd(0.05), "$0.05");
    }

    #[test]
    fn format_price_shapes() {
        assert_eq!(format_price(None, false), "free");
        assert_eq!(format_price(None, true), "metered");
        assert_eq!(format_price(Some(&json!({"usd": 0.001})), true), "$0.001");
        assert_eq!(format_price(Some(&json!({"usd": 0.0})), true), "free");
        assert_eq!(
            format_price(
                Some(&json!({"subscription": {"period": "30d", "price_usd": 9.99}})),
                true
            ),
            "$9.99 / 30d"
        );
        // public catalog flat-per-request shape
        assert_eq!(
            format_price(
                Some(
                    &json!({"dimensions":[{"unit":"requests","scale":1,"tiers":[{"price_usd":0.001}]}]})
                ),
                true
            ),
            "$0.001 / req"
        );
        // tokens with scale + direction
        assert_eq!(
            format_price(
                Some(
                    &json!({"dimensions":[{"unit":"tokens","scale":1000000,"direction":"input","tiers":[{"price_usd":5.0}]}]})
                ),
                true
            ),
            "$5 in / 1M tok"
        );
        // tiered → price range
        assert_eq!(
            format_price(
                Some(
                    &json!({"dimensions":[{"unit":"requests","scale":1,"tiers":[{"price_usd":1.0},{"price_usd":2.0}]}]})
                ),
                true
            ),
            "$1–$2 / req"
        );
        // multi-dimension (input + output) joined
        assert_eq!(
            format_price(
                Some(&json!({"dimensions":[
                    {"unit":"tokens","scale":1000000,"direction":"input","tiers":[{"price_usd":0.5}]},
                    {"unit":"tokens","scale":1000000,"direction":"output","tiers":[{"price_usd":1.5}]}
                ]})),
                true
            ),
            "$0.5 in / 1M tok  +  $1.5 out / 1M tok"
        );
    }
}
