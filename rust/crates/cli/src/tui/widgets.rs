//! Generic, reusable rendering widgets shared by the TUI flows: sliders,
//! QR codes, the Solana logo, sidebar option cards, the bottom controls bar,
//! and small animations.

use qrcode::{Color as QrColor, QrCode};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::theme::{SOLANA_BLUE, SOLANA_GREEN, SOLANA_PURPLE, TOPUP_CARD_BG, TOPUP_MAIN_BG, TOPUP_SIDEBAR_BG};

/// Cell glyph used to draw slider tracks.
const SLIDER_CELL: &str = "▐";

// ── QR rendering ─────────────────────────────────────────────────────────

pub(crate) struct RenderedQr {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

pub(crate) fn render_qr(
    data: &str,
    max_width: u16,
    max_height: u16,
) -> Result<Option<RenderedQr>, qrcode::types::QrError> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::L)?;
    Ok(render_qr_code(&code, max_width, max_height))
}

pub(crate) fn render_qr_code(code: &QrCode, max_width: u16, max_height: u16) -> Option<RenderedQr> {
    let modules = code.width();
    let (module_cols, module_subrows) = choose_qr_module_cells(modules, max_width, max_height)?;

    let scaled_rows = modules * module_subrows;
    let mut lines = Vec::with_capacity(scaled_rows.div_ceil(2));
    for top_subrow in (0..scaled_rows).step_by(2) {
        let mut spans = Vec::with_capacity(modules);
        for x in 0..modules {
            let top_dark = qr_subrow_dark(code, x, top_subrow, module_subrows);
            let bottom_dark = qr_subrow_dark(code, x, top_subrow + 1, module_subrows);
            spans.push(render_qr_half_block(top_dark, bottom_dark, module_cols));
        }

        lines.push(Line::from(spans));
    }
    let width = lines.first().map(Line::width).unwrap_or(0) as u16;
    let height = lines.len() as u16;

    Some(RenderedQr {
        lines,
        width,
        height,
    })
}

fn qr_subrow_dark(code: &QrCode, x: usize, subrow: usize, module_subrows: usize) -> bool {
    let y = subrow / module_subrows;
    y < code.width() && code[(x, y)] != QrColor::Light
}

fn render_qr_half_block(top_dark: bool, bottom_dark: bool, module_cols: usize) -> Span<'static> {
    let cells = match (top_dark, bottom_dark) {
        (true, true) => " ".repeat(module_cols),
        (true, false) => "▀".repeat(module_cols),
        (false, true) => "▄".repeat(module_cols),
        (false, false) => " ".repeat(module_cols),
    };

    let style = match (top_dark, bottom_dark) {
        (true, true) => Style::default().bg(Color::White),
        (true, false) | (false, true) => Style::default().fg(Color::White).bg(TOPUP_MAIN_BG),
        (false, false) => Style::default().bg(TOPUP_MAIN_BG),
    };

    Span::styled(cells, style)
}

fn choose_qr_module_cells(
    modules: usize,
    max_width: u16,
    max_height: u16,
) -> Option<(usize, usize)> {
    let max_cols = (usize::from(max_width) / modules).min(8);
    let max_subrows = ((usize::from(max_height) * 2) / modules).min(8);
    let module_size = max_cols.min(max_subrows);

    (module_size > 0).then_some((module_size, module_size))
}

/// Fallback rendering when the terminal is too small to fit the QR code.
pub(crate) fn unavailable_qr() -> RenderedQr {
    let lines = ["Make this window larger", "to show the QR code"]
        .into_iter()
        .map(|text| Line::from(Span::styled(text, Style::default().fg(Color::DarkGray))).centered())
        .collect::<Vec<_>>();
    RenderedQr {
        width: lines.iter().map(Line::width).max().unwrap_or(0) as u16,
        height: lines.len() as u16,
        lines,
    }
}

// ── Solana logo ──────────────────────────────────────────────────────────

pub(crate) fn solana_logo(prefix: &'static str) -> Vec<Line<'static>> {
    vec![
        solana_logo_line(
            prefix,
            "⣠⣶",
            SOLANA_BLUE,
            "⣶⣶",
            SOLANA_GREEN,
            "⣶⣶⠖",
            SOLANA_GREEN,
        ),
        solana_logo_line(
            prefix,
            "⠲⣶",
            SOLANA_PURPLE,
            "⣶⣶",
            SOLANA_BLUE,
            "⣶⣶⣄",
            SOLANA_GREEN,
        ),
        solana_logo_line(
            prefix,
            "⣠⣶",
            SOLANA_PURPLE,
            "⣶⣶",
            SOLANA_PURPLE,
            "⣶⣶⠖",
            SOLANA_BLUE,
        ),
    ]
}

fn solana_logo_line(
    prefix: &'static str,
    left: &'static str,
    left_color: Color,
    middle: &'static str,
    middle_color: Color,
    right: &'static str,
    right_color: Color,
) -> Line<'static> {
    Line::from(vec![
        Span::raw(prefix),
        Span::styled(left, Style::default().fg(left_color)),
        Span::styled(middle, Style::default().fg(middle_color)),
        Span::styled(right, Style::default().fg(right_color)),
    ])
}

// ── Sliders ──────────────────────────────────────────────────────────────

/// Interpolate bar color from green → yellow → red based on position.
pub(crate) fn bar_color(index: usize, total: usize, bright: bool) -> Color {
    if index == 0 {
        return if bright {
            Color::Rgb(180, 180, 185)
        } else {
            Color::Rgb(110, 110, 115)
        };
    }

    let t = index as f64 / total.max(1) as f64;

    let (r, g) = if t < 0.5 {
        let s = t * 2.0;
        (s, 1.0)
    } else {
        let s = (t - 0.5) * 2.0;
        (1.0, 1.0 - s)
    };

    if bright {
        Color::Rgb((r * 255.0) as u8, (g * 255.0) as u8, 40)
    } else {
        Color::Rgb((r * 140.0) as u8, (g * 140.0) as u8, 30)
    }
}

fn render_scale_spans(
    bar_width: usize,
    max_steps: usize,
    track_last: usize,
    labels: &[(usize, &str)],
) -> Vec<Span<'static>> {
    let arrow_width = 3usize;
    let mut chars = vec![' '; bar_width];

    for &(position, label) in labels {
        let label_width = label.chars().count();
        let track_pos = (position.min(max_steps) * track_last)
            .checked_div(max_steps)
            .unwrap_or(0);
        let label_center = arrow_width + track_pos;
        let preferred_start = label_center.saturating_sub(label_width / 2);
        let label_start = if bar_width <= label_width {
            0
        } else {
            let bar_max_start = bar_width.saturating_sub(label_width);
            let track_start = arrow_width.min(bar_max_start);
            let track_end = arrow_width
                .saturating_add(track_last)
                .min(bar_width.saturating_sub(1));

            if track_end >= track_start.saturating_add(label_width.saturating_sub(1)) {
                preferred_start.clamp(
                    track_start,
                    track_end.saturating_sub(label_width.saturating_sub(1)),
                )
            } else {
                preferred_start.min(bar_max_start)
            }
        };

        for (idx, ch) in label.chars().enumerate() {
            if let Some(slot) = chars.get_mut(label_start + idx) {
                *slot = ch;
            }
        }
    }

    vec![Span::styled(
        chars.into_iter().collect::<String>(),
        Style::default().fg(Color::DarkGray),
    )]
}

/// Generic slider bar used by both the session budget box and the topup amount box.
pub(crate) fn render_slider_box<'a>(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: impl Into<ratatui::widgets::block::Title<'a>>,
    position: usize,
    max_steps: usize,
    scale_labels: &[(usize, &str)],
    focused: bool,
) {
    let border_color = if focused {
        Color::Green
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let box_width = area.width;
    let bar_width = (box_width as usize).saturating_sub(4);
    let track_width = bar_width.saturating_sub(6); // account for arrows
    let track_last = track_width.saturating_sub(1);
    let cursor_pos = (position.min(max_steps) * track_last)
        .checked_div(max_steps)
        .unwrap_or(0);

    let arrow_style = Style::default().fg(Color::Cyan).bold();
    let mut bar_spans = vec![Span::styled(" ◀ ", arrow_style)];
    for i in 0..bar_width.saturating_sub(6) {
        let color = if i == cursor_pos {
            bar_color(i, bar_width.saturating_sub(6), true)
        } else if i < cursor_pos {
            bar_color(i, bar_width.saturating_sub(6), false)
        } else {
            Color::Rgb(50, 55, 60)
        };
        bar_spans.push(Span::styled("▐", Style::default().fg(color)));
    }
    bar_spans.push(Span::styled(" ▶ ", arrow_style));

    let lines = vec![
        Line::default(),
        Line::from(bar_spans),
        Line::from(render_scale_spans(
            bar_width,
            max_steps,
            track_last,
            scale_labels,
        )),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Borderless slider (used in the topup QR view). Three rows: centered title,
/// the bar with arrows, and the scale labels. No surrounding box —
/// the caller positions and sizes `area` to control the visual width.
pub(crate) fn render_slider<'a>(
    frame: &mut ratatui::Frame,
    area: Rect,
    title_spans: Vec<Span<'a>>,
    position: usize,
    max_steps: usize,
    scale_labels: &[(usize, &str)],
) {
    let bar_width = area.width as usize;
    let track_width = bar_width.saturating_sub(6); // 3 chars per arrow
    let track_last = track_width.saturating_sub(1);
    let cursor_pos = (position.min(max_steps) * track_last)
        .checked_div(max_steps)
        .unwrap_or(0);

    let arrow_style = Style::default().fg(Color::Cyan).bold();
    let mut bar_spans = vec![Span::styled(" ◀ ", arrow_style)];
    for i in 0..track_width {
        let color = if i == cursor_pos {
            bar_color(i, track_width, true)
        } else if i < cursor_pos {
            bar_color(i, track_width, false)
        } else {
            Color::Rgb(50, 55, 60)
        };
        bar_spans.push(Span::styled(SLIDER_CELL, Style::default().fg(color)));
    }
    bar_spans.push(Span::styled(" ▶ ", arrow_style));

    let lines = vec![
        Line::from(title_spans).centered(),
        Line::from(bar_spans),
        Line::from(render_scale_spans(
            bar_width,
            max_steps,
            track_last,
            scale_labels,
        )),
    ];

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );
}

// ── Sidebar cards + controls bar ─────────────────────────────────────────

/// 3-row sidebar option card: full background fill (accent color when
/// selected, dark card gray otherwise), no border, bold centered title on
/// the middle row, optional dimmed subtitle lines below it.
pub(crate) fn sidebar_card(
    area: Rect,
    frame: &mut ratatui::Frame,
    title: &str,
    subtitle_lines: &[&str],
    accent_color: Color,
    selected: bool,
) {
    let bg = if selected { accent_color } else { TOPUP_CARD_BG };
    let title_color = if selected { Color::White } else { Color::Gray };
    let block = Block::default().style(Style::default().bg(bg));
    // Card height is 3 — pad a blank line above the title so it lands
    // on the middle row.
    let mut lines = vec![
        Line::default(),
        Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
        .centered(),
    ];
    for subtitle in subtitle_lines {
        lines.push(
            Line::from(Span::styled(
                (*subtitle).to_string(),
                Style::default().fg(Color::Gray),
            ))
            .centered(),
        );
    }
    let card = Paragraph::new(lines).block(block);
    frame.render_widget(card, area);
}

/// One-row bottom controls bar: `(key, label)` entries on the left
/// (highlighted key + dimmed label, `│`-separated) and an optional
/// right-aligned status line.
pub(crate) fn controls_bar(
    frame: &mut ratatui::Frame,
    area: Rect,
    entries: &[(&str, &str)],
    right_status: Option<Line>,
) {
    // Key colors follow the existing convention: destructive keys (Esc)
    // red, confirmation keys (Enter) green, navigation keys cyan.
    let key_style = |key: &str| match key {
        "Esc" => Style::default().fg(Color::Red).bold(),
        "Enter" => Style::default().fg(Color::Green).bold(),
        _ => Style::default().fg(Color::Cyan).bold(),
    };

    let mut spans = Vec::with_capacity(entries.len() * 2 + 1);
    for (idx, (key, label)) in entries.iter().enumerate() {
        spans.push(Span::styled(key.to_string(), key_style(key)));
        let text = if idx + 1 < entries.len() {
            format!(" {label}  │  ")
        } else {
            format!(" {label}")
        };
        spans.push(Span::styled(text, Style::default().dim()));
    }

    if let Some(status) = right_status {
        let controls_width: usize = spans.iter().map(|span| span.content.len()).sum();
        let status_width: usize = status
            .spans
            .iter()
            .map(|span| span.content.len())
            .sum();
        let total_width = controls_width.saturating_add(status_width);
        let gap = (area.width as usize).saturating_sub(total_width);
        spans.push(Span::raw(" ".repeat(gap.max(1))));
        spans.extend(status.spans);
    }

    let line = Line::from(spans);

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(TOPUP_SIDEBAR_BG)),
        area,
    );
}

// ── Animations ───────────────────────────────────────────────────────────

pub(crate) fn render_success_checkmark(frame: &mut ratatui::Frame, area: Rect, visible: bool) {
    frame.render_widget(Clear, area);

    let g = Style::default().fg(Color::Green).bold();

    let checkmark: Vec<Line> = if visible {
        vec![
            Line::raw(""),
            Line::styled("                              ████", g),
            Line::styled("                            ██████", g),
            Line::styled("                          ████████", g),
            Line::styled("                        ████████  ", g),
            Line::styled("                      ████████    ", g),
            Line::styled("                    ████████      ", g),
            Line::styled("                  ████████        ", g),
            Line::styled("                ████████          ", g),
            Line::styled("  ████        ████████            ", g),
            Line::styled("  ██████    ████████              ", g),
            Line::styled("  ████████████████                ", g),
            Line::styled("    ████████████                  ", g),
            Line::styled("      ████████                    ", g),
            Line::styled("        ████                      ", g),
            Line::raw(""),
        ]
    } else {
        vec![Line::raw(""); 16]
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let top_pad = inner.height.saturating_sub(checkmark.len() as u16) / 2;
    let text_area = Rect {
        x: inner.x,
        y: inner.y + top_pad,
        width: inner.width,
        height: inner.height.saturating_sub(top_pad),
    };
    frame.render_widget(
        Paragraph::new(checkmark).alignment(ratatui::layout::Alignment::Center),
        text_area,
    );
}

/// Vertical "money is flowing in" animation rendered beneath the active
/// payment-method button. Each row shows either a bright `▼`, a dim `▽`,
/// or a blank, with the bright glyph drifting downward as `tick`
/// advances — giving the impression of falling drops in the brand colour.
pub(crate) fn render_money_flow(frame: &mut ratatui::Frame, area: Rect, color: Color, tick: usize) {
    let height = area.height as usize;
    if height == 0 {
        return;
    }
    // Cycle length controls drop spacing: longer = sparser drops. Adding
    // a couple of empty phases past the visible rows keeps the column
    // from looking constantly full.
    let cycle = (height + 2).max(6) as i32;
    // Spinner ticks every ~80ms. Slow the drop ~3× so each row dwells
    // for ~240ms — money trickling in, not raining.
    const FLOW_DAMPING: i32 = 3;
    let slow_tick = (tick as i32) / FLOW_DAMPING;
    let bright = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(color).add_modifier(Modifier::DIM);

    let lines: Vec<Line<'static>> = (0..height)
        .map(|row| {
            // phase increases as slow_tick increases; subtracting `row`
            // makes higher rows lead the cycle, so the drop falls.
            let phase = ((slow_tick - row as i32).rem_euclid(cycle)) as usize;
            let span = match phase {
                0 => Span::styled("▼", bright),
                1 => Span::styled("▽", dim),
                _ => Span::raw(" "),
            };
            Line::from(span).centered()
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_SOLANA_PAY_URL: &str = "solana:11111111111111111111111111111111?amount=5&spl-token=\
         EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    #[test]
    fn topup_slider_cell_is_single_char_slot() {
        assert_eq!(SLIDER_CELL, "▐");
        assert_eq!(SLIDER_CELL.chars().count(), 1);
    }

    #[test]
    fn render_scale_spans_keeps_edge_labels_inside_track() {
        // 25 = the topup slider's max step count (TOPUP_MAX_STEPS).
        let spans = render_scale_spans(40, 25, 33, &[(0, "any"), (25, "$25")]);
        let line = spans[0].content.as_ref();

        assert_eq!(line.len(), 40);
        assert_eq!(&line[0..3], "   ");
        assert_eq!(&line[3..6], "any");
        assert_eq!(&line[34..37], "$25");
        assert_eq!(&line[37..40], "   ");
    }

    #[test]
    fn topup_qr_render_keeps_square_physical_geometry() {
        let qr = render_qr(SAMPLE_SOLANA_PAY_URL, 120, 60)
            .expect("QR should encode")
            .expect("QR should fit");

        assert!(qr.width <= 120);
        assert!(qr.height <= 60);

        let physical_width = usize::from(qr.width);
        let physical_height = usize::from(qr.height) * 2;
        assert!(physical_width.abs_diff(physical_height) <= 1);
    }

    #[test]
    fn topup_qr_render_defaults_to_two_column_modules() {
        let qr = render_qr(SAMPLE_SOLANA_PAY_URL, 120, 60)
            .expect("QR should encode")
            .expect("QR should fit");

        let physical_width = usize::from(qr.width);
        let physical_height = usize::from(qr.height) * 2;
        assert!(physical_width.abs_diff(physical_height) <= 1);
    }

    #[test]
    fn topup_qr_render_fits_compact_terminal_area() {
        let qr = render_qr(SAMPLE_SOLANA_PAY_URL, 120, 30)
            .expect("QR should encode")
            .expect("QR should fit");

        assert!(qr.width <= 120);
        assert!(qr.height <= 30);
    }

    #[test]
    fn topup_qr_render_refuses_to_clip() {
        let qr = render_qr(SAMPLE_SOLANA_PAY_URL, 1, 1).expect("QR should encode");

        assert!(qr.is_none());
    }

    #[test]
    fn unavailable_qr_asks_user_to_resize_window() {
        let qr = unavailable_qr();
        let text = qr
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(text, vec!["Make this window larger", "to show the QR code"]);
        assert_eq!(qr.width, "Make this window larger".len() as u16);
        assert_eq!(qr.height, 2);
    }
}
