//! Harness-agnostic provider picker TUI: choose the inference provider (and
//! model) a coding harness should talk to through the Pay gateway.
//!
//! Built to scale past local discovery — providers will eventually come from
//! a DHT registry with many entries — so the layout is a full-width plain
//! table (no sidebar, no borders): a model-filter chip row, a type-to-search
//! field, and a rail-styled provider list in the notice-component style
//! (`components/notice.rs`: colored `│` rail + bold title + dimmed body).

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::commands::server::inference::discovery::DiscoveredProvider;

use super::term::{SPINNER, TuiBackend, with_terminal};
use super::theme::{TOPUP_CARD_BG, TOPUP_MAIN_BG};
use super::widgets::controls_bar;

/// Rows per provider entry: two content lines + one blank separator.
const ENTRY_HEIGHT: u16 = 3;

/// The provider (and model) the user picked.
pub struct ProviderChoice {
    pub provider: DiscoveredProvider,
    pub model: String,
}

// One short-lived value per TUI invocation — boxing the payload buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum ProviderSelection {
    Selected(ProviderChoice),
    Cancelled,
}

/// Run the picker for `harness` (display-only label, e.g. the harness
/// command name). `requested_model` locks the model choice when set.
pub fn select_provider(
    harness: &str,
    providers: Vec<DiscoveredProvider>,
    requested_model: Option<&str>,
) -> io::Result<ProviderSelection> {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) || providers.is_empty() {
        return Ok(ProviderSelection::Cancelled);
    }

    let harness = harness.to_string();
    let requested_model = requested_model.map(str::to_string);
    with_terminal(|terminal| run(terminal, harness, providers, requested_model))
}

fn run(
    terminal: &mut Terminal<TuiBackend>,
    harness: String,
    providers: Vec<DiscoveredProvider>,
    requested_model: Option<String>,
) -> io::Result<ProviderSelection> {
    let mut app = ProviderPickerApp::new(harness, providers, requested_model);

    loop {
        app.tick = app.tick.wrapping_add(1);
        terminal.draw(|frame| render(frame, &app))?;

        if event::poll(std::time::Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Esc => return Ok(ProviderSelection::Cancelled),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(ProviderSelection::Cancelled);
                }
                KeyCode::Up => app.move_selection(-1),
                KeyCode::Down => app.move_selection(1),
                KeyCode::Tab | KeyCode::Right => app.cycle_model_filter(1),
                KeyCode::BackTab | KeyCode::Left => app.cycle_model_filter(-1),
                KeyCode::Enter => {
                    if let Some(choice) = app.choice() {
                        return Ok(ProviderSelection::Selected(choice));
                    }
                }
                KeyCode::Backspace => app.pop_search(),
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    app.push_search(ch);
                }
                _ => {}
            }
        }
    }
}

// ── App state ─────────────────────────────────────────────────────────────

struct ProviderPickerApp {
    /// Display-only harness label for headings/status.
    harness: String,
    providers: Vec<DiscoveredProvider>,
    /// Distinct model names across all providers, first-appearance order.
    model_names: Vec<String>,
    /// Active model-filter chip: `None` = "all", else index into
    /// [`Self::model_names`].
    model_filter: Option<usize>,
    /// Live type-to-search query (case-insensitive substring).
    search: String,
    /// Index into [`Self::filtered`].
    selected: usize,
    /// `--model` lock: overrides both filter and per-provider choice.
    requested_model: Option<String>,
    tick: usize,
}

impl ProviderPickerApp {
    fn new(
        harness: String,
        providers: Vec<DiscoveredProvider>,
        requested_model: Option<String>,
    ) -> Self {
        let mut model_names: Vec<String> = Vec::new();
        for provider in &providers {
            for model in &provider.models {
                if !model_names.contains(model) {
                    model_names.push(model.clone());
                }
            }
        }
        Self {
            harness,
            providers,
            model_names,
            model_filter: None,
            search: String::new(),
            selected: 0,
            requested_model,
            tick: 0,
        }
    }

    fn model_locked(&self) -> bool {
        self.requested_model.is_some()
    }

    /// Name of the active model-filter chip, if any.
    fn active_model(&self) -> Option<&str> {
        self.model_filter
            .and_then(|idx| self.model_names.get(idx))
            .map(String::as_str)
    }

    /// Case-insensitive substring match over title, slug, and model names,
    /// intersected with the active model filter.
    fn matches(&self, provider: &DiscoveredProvider) -> bool {
        if let Some(model) = self.active_model()
            && !provider.models.iter().any(|m| m == model)
        {
            return false;
        }
        if self.search.is_empty() {
            return true;
        }
        let query = self.search.to_lowercase();
        provider.title().to_lowercase().contains(&query)
            || provider.slug().to_lowercase().contains(&query)
            || provider
                .models
                .iter()
                .any(|m| m.to_lowercase().contains(&query))
    }

    /// Providers passing the search + model filters, input order.
    fn filtered(&self) -> Vec<&DiscoveredProvider> {
        self.providers.iter().filter(|p| self.matches(p)).collect()
    }

    fn selected_provider(&self) -> Option<&DiscoveredProvider> {
        self.filtered().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.filtered().len().saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        let next = (self.selected as isize + delta).clamp(0, len as isize - 1);
        self.selected = next as usize;
    }

    /// Cycle the model-filter chips: all → each model → all (wraps both
    /// ways). No-op while the model is locked by `--model`.
    fn cycle_model_filter(&mut self, delta: isize) {
        if self.model_locked() || self.model_names.is_empty() {
            return;
        }
        let total = self.model_names.len() as isize + 1; // + "all"
        let current = self.model_filter.map_or(0, |idx| idx as isize + 1);
        let next = (current + delta).rem_euclid(total);
        self.model_filter = (next > 0).then(|| (next - 1) as usize);
        self.clamp_selection();
    }

    fn push_search(&mut self, ch: char) {
        self.search.push(ch);
        self.clamp_selection();
    }

    fn pop_search(&mut self) {
        self.search.pop();
        self.clamp_selection();
    }

    /// Model that Enter would launch for `provider`:
    /// `--model` lock > active model-filter chip > the provider's first
    /// model. `None` when the provider reports no models and nothing else
    /// decides.
    fn chosen_model(&self, provider: &DiscoveredProvider) -> Option<String> {
        if let Some(model) = &self.requested_model {
            return Some(model.clone());
        }
        if let Some(model) = self.active_model()
            && provider.models.iter().any(|m| m == model)
        {
            return Some(model.to_string());
        }
        provider.models.first().cloned()
    }

    fn choice(&self) -> Option<ProviderChoice> {
        let provider = self.selected_provider()?.clone();
        let model = self.chosen_model(&provider)?;
        Some(ProviderChoice { provider, model })
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────

fn render(frame: &mut ratatui::Frame, app: &ProviderPickerApp) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );

    let rows = Layout::vertical([
        Constraint::Length(1), // heading
        Constraint::Length(1), // model chips
        Constraint::Length(1), // search
        Constraint::Length(1), // gap
        Constraint::Min(0),    // provider list
        Constraint::Length(1), // controls
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("PROVIDER FOR {}", app.harness.to_uppercase()),
            Style::default().fg(Color::DarkGray).bold(),
        ))),
        inset_x(rows[0], 2),
    );
    render_model_chips(frame, inset_x(rows[1], 2), app);
    render_search(frame, inset_x(rows[2], 2), app);
    render_provider_list(frame, inset_x(rows[4], 2), app);
    render_controls(frame, rows[5], app);
}

/// Model-filter chip row: `model  all  llama3.2  qwen3 …` with the active
/// chip highlighted. Windows from the left so the active chip stays
/// visible when there are many models.
fn render_model_chips(frame: &mut ratatui::Frame, area: Rect, app: &ProviderPickerApp) {
    let dim = Style::default().fg(Color::DarkGray);
    if app.model_locked() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("model ", dim),
                Span::styled(
                    app.requested_model.clone().unwrap_or_default(),
                    Style::default().fg(Color::White).bold(),
                ),
                Span::styled("  locked by --model", dim),
            ])),
            area,
        );
        return;
    }

    let mut chips: Vec<(&str, bool)> = vec![("all", app.model_filter.is_none())];
    for (idx, name) in app.model_names.iter().enumerate() {
        chips.push((name.as_str(), app.model_filter == Some(idx)));
    }

    // Drop leading chips until the active one fits in the row.
    let avail = (area.width as usize).saturating_sub("model ".len() + 2);
    let chip_width = |name: &str| name.chars().count() + 3; // " name " + gap
    let active_idx = chips.iter().position(|(_, active)| *active).unwrap_or(0);
    let mut start = 0;
    while start < active_idx
        && chips[start..=active_idx]
            .iter()
            .map(|(name, _)| chip_width(name))
            .sum::<usize>()
            > avail
    {
        start += 1;
    }

    let mut spans = vec![Span::styled("model ", dim)];
    if start > 0 {
        spans.push(Span::styled("… ", dim));
    }
    for (name, active) in chips.iter().skip(start) {
        if *active {
            spans.push(Span::styled(
                format!(" {name} "),
                Style::default().fg(Color::White).bg(TOPUP_CARD_BG).bold(),
            ));
        } else {
            spans.push(Span::styled(format!(" {name} "), dim));
        }
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Type-to-search field: the live query with a cursor, or a dim
/// placeholder while empty.
fn render_search(frame: &mut ratatui::Frame, area: Rect, app: &ProviderPickerApp) {
    let line = if app.search.is_empty() {
        Line::from(vec![
            Span::styled("▌", Style::default().fg(Color::Gray)),
            Span::styled("search providers…", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled(app.search.clone(), Style::default().fg(Color::White).bold()),
            Span::styled("▌", Style::default().fg(Color::Gray)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_provider_list(frame: &mut ratatui::Frame, area: Rect, app: &ProviderPickerApp) {
    let providers = app.filtered();
    if providers.is_empty() {
        if area.height > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no providers match",
                    Style::default().fg(Color::DarkGray),
                ))),
                Rect { height: 1, ..area },
            );
        }
        return;
    }

    let visible = (area.height / ENTRY_HEIGHT) as usize;
    if visible == 0 {
        return;
    }
    let offset = app.selected.saturating_sub(visible.saturating_sub(1));

    let mut y = area.y;
    for (idx, provider) in providers.iter().enumerate().skip(offset).take(visible) {
        let entry_area = Rect {
            x: area.x,
            y,
            width: area.width,
            height: ENTRY_HEIGHT,
        };
        render_provider_entry(frame, entry_area, provider, idx == app.selected);
        y += ENTRY_HEIGHT;
    }
}

/// One two-line entry in the notice style: a brand-colored `│` rail
/// spanning both lines, bold title + dim origin on line 1, dim model list
/// on line 2. Selection thickens the rail (`┃`) and brightens the title.
fn render_provider_entry(
    frame: &mut ratatui::Frame,
    area: Rect,
    provider: &DiscoveredProvider,
    selected: bool,
) {
    let accent = provider
        .color()
        .and_then(hex_color)
        .unwrap_or(Color::DarkGray);
    let rail_glyph = if selected { "┃" } else { "│" };
    let rail = Span::styled(format!("{rail_glyph} "), Style::default().fg(accent).bold());
    let title_style = if selected {
        Style::default().fg(Color::White).bold()
    } else {
        Style::default().fg(Color::Gray).bold()
    };
    let dim = Style::default().fg(Color::DarkGray);

    let mut top = vec![
        rail.clone(),
        Span::styled(provider.title().to_string(), title_style),
        Span::styled(format!("  {}", provider.base_url), dim),
    ];
    if let Some(version) = provider.version.as_deref() {
        top.push(Span::styled(format!(" · v{version}"), dim));
    }

    let count = provider.models.len();
    let noun = if count == 1 { "model" } else { "models" };
    let models = if provider.models.is_empty() {
        "no models reported".to_string()
    } else {
        provider.models.join(", ")
    };
    // Rail (2) + " · N models" suffix budget (12).
    let models_width = (area.width as usize).saturating_sub(14);
    let bottom = vec![
        rail,
        Span::styled(truncate_str(&models, models_width), dim),
        Span::styled(format!(" · {count} {noun}"), dim),
    ];

    frame.render_widget(
        Paragraph::new(vec![Line::from(top), Line::from(bottom)]),
        area,
    );
}

fn render_controls(frame: &mut ratatui::Frame, area: Rect, app: &ProviderPickerApp) {
    let entries: Vec<(&str, &str)> = vec![
        ("↑ ↓", "select"),
        ("⇥", "model filter"),
        ("a-z", "search"),
        ("⏎", "select"),
        ("Esc", "cancel"),
    ];

    let spinner = SPINNER[app.tick % SPINNER.len()];
    let status = match app.selected_provider() {
        Some(provider) => match app.chosen_model(provider) {
            Some(model) => Line::from(vec![
                Span::styled(spinner, Style::default().fg(Color::Green).bold()),
                Span::styled(
                    format!(" {} → {}/{} ", app.harness, provider.slug(), model),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            None => Line::from(Span::styled(
                "pass --model to launch this provider ",
                Style::default().fg(Color::Yellow),
            )),
        },
        None => Line::from(Span::styled(
            "no providers match ",
            Style::default().fg(Color::DarkGray),
        )),
    };
    controls_bar(frame, area, &entries, Some(status));
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn inset_x(area: Rect, x: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(x),
        width: area.width.saturating_sub(x.saturating_mul(2)),
        ..area
    }
}

/// `#rrggbb` → `Color::Rgb`.
fn hex_color(hex: &str) -> Option<Color> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

/// Truncate to at most `max` characters, ellipsizing when clipped.
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commands::server::inference::providers::lm_studio::LmStudio;
    use crate::commands::server::inference::providers::ollama::Ollama;

    fn ollama(models: &[&str]) -> DiscoveredProvider {
        DiscoveredProvider {
            provider: Arc::new(Ollama),
            base_url: "http://127.0.0.1:11434".into(),
            models: models.iter().map(|m| (*m).to_string()).collect(),
            version: Some("0.9.1".into()),
        }
    }

    fn lm_studio(models: &[&str]) -> DiscoveredProvider {
        DiscoveredProvider {
            provider: Arc::new(LmStudio),
            base_url: "http://127.0.0.1:1234".into(),
            models: models.iter().map(|m| (*m).to_string()).collect(),
            version: None,
        }
    }

    fn app(providers: Vec<DiscoveredProvider>) -> ProviderPickerApp {
        ProviderPickerApp::new("codex".into(), providers, None)
    }

    fn filtered_slugs(app: &ProviderPickerApp) -> Vec<&str> {
        app.filtered().iter().map(|p| p.slug()).collect()
    }

    // ── Filtering ──

    #[test]
    fn search_matches_title_slug_and_models_case_insensitively() {
        let mut app = app(vec![ollama(&["llama3.2:3b"]), lm_studio(&["qwen2.5-7b"])]);
        assert_eq!(filtered_slugs(&app), vec!["ollama", "lm-studio"]);

        // Title match, case-insensitive.
        app.search = "OLLAMA".into();
        assert_eq!(filtered_slugs(&app), vec!["ollama"]);

        // Slug match.
        app.search = "lm-stu".into();
        assert_eq!(filtered_slugs(&app), vec!["lm-studio"]);

        // Model-name match.
        app.search = "QWEN".into();
        assert_eq!(filtered_slugs(&app), vec!["lm-studio"]);

        // No match.
        app.search = "nope".into();
        assert!(app.filtered().is_empty());
        assert!(app.selected_provider().is_none());
    }

    #[test]
    fn model_filter_narrows_to_providers_serving_that_model() {
        let mut app = app(vec![
            ollama(&["llama3.2:3b", "shared-model"]),
            lm_studio(&["qwen2.5-7b", "shared-model"]),
        ]);
        // Distinct models, first-appearance order.
        assert_eq!(
            app.model_names,
            vec!["llama3.2:3b", "shared-model", "qwen2.5-7b"]
        );

        // all → llama3.2:3b (only ollama serves it).
        app.cycle_model_filter(1);
        assert_eq!(app.active_model(), Some("llama3.2:3b"));
        assert_eq!(filtered_slugs(&app), vec!["ollama"]);

        // shared-model → both providers.
        app.cycle_model_filter(1);
        assert_eq!(app.active_model(), Some("shared-model"));
        assert_eq!(filtered_slugs(&app), vec!["ollama", "lm-studio"]);

        // qwen → lm-studio only; then wraps back to "all".
        app.cycle_model_filter(1);
        assert_eq!(filtered_slugs(&app), vec!["lm-studio"]);
        app.cycle_model_filter(1);
        assert_eq!(app.active_model(), None);
        assert_eq!(filtered_slugs(&app), vec!["ollama", "lm-studio"]);

        // Backwards from "all" wraps to the last model.
        app.cycle_model_filter(-1);
        assert_eq!(app.active_model(), Some("qwen2.5-7b"));
    }

    #[test]
    fn search_and_model_filter_combine() {
        let mut app = app(vec![
            ollama(&["shared-model"]),
            lm_studio(&["shared-model", "qwen2.5-7b"]),
        ]);
        // Model filter alone keeps both…
        app.cycle_model_filter(1); // shared-model
        assert_eq!(app.active_model(), Some("shared-model"));
        assert_eq!(filtered_slugs(&app), vec!["ollama", "lm-studio"]);

        // …the search then narrows within the model filter.
        app.search = "studio".into();
        assert_eq!(filtered_slugs(&app), vec!["lm-studio"]);
    }

    #[test]
    fn selection_clamps_as_filters_narrow() {
        let mut app = app(vec![ollama(&["llama3.2:3b"]), lm_studio(&["qwen2.5-7b"])]);
        app.move_selection(1);
        assert_eq!(app.selected, 1);
        assert_eq!(app.selected_provider().unwrap().slug(), "lm-studio");

        // Typing narrows the list to one entry — selection clamps to it.
        for ch in "ollama".chars() {
            app.push_search(ch);
        }
        assert_eq!(app.selected, 0);
        assert_eq!(app.selected_provider().unwrap().slug(), "ollama");

        // Deleting the query restores the full list; selection stays valid.
        while !app.search.is_empty() {
            app.pop_search();
        }
        assert_eq!(filtered_slugs(&app).len(), 2);
        assert_eq!(app.selected, 0);
    }

    // ── Enter → choice ──

    #[test]
    fn enter_defaults_to_the_selected_providers_first_model() {
        let app = app(vec![ollama(&["llama3.2:3b", "nomic-embed"])]);
        let choice = app.choice().unwrap();
        assert_eq!(choice.provider.slug(), "ollama");
        assert_eq!(choice.model, "llama3.2:3b");
    }

    #[test]
    fn active_model_filter_pre_picks_that_model() {
        let mut app = app(vec![ollama(&["llama3.2:3b", "nomic-embed"])]);
        app.cycle_model_filter(1); // llama3.2:3b
        app.cycle_model_filter(1); // nomic-embed
        assert_eq!(app.active_model(), Some("nomic-embed"));
        assert_eq!(app.choice().unwrap().model, "nomic-embed");
    }

    #[test]
    fn requested_model_locks_the_choice_and_the_filter() {
        let mut app = ProviderPickerApp::new(
            "codex".into(),
            vec![ollama(&["llama3.2:3b"])],
            Some("custom-model".into()),
        );
        assert!(app.model_locked());
        assert_eq!(app.choice().unwrap().model, "custom-model");

        // Cycling the filter is a no-op while locked.
        app.cycle_model_filter(1);
        assert_eq!(app.model_filter, None);
        assert_eq!(app.choice().unwrap().model, "custom-model");
    }

    #[test]
    fn provider_without_models_yields_no_choice_unless_locked() {
        let app = app(vec![ollama(&[])]);
        assert!(app.choice().is_none());

        let locked =
            ProviderPickerApp::new("codex".into(), vec![ollama(&[])], Some("custom".into()));
        assert_eq!(locked.choice().unwrap().model, "custom");
    }

    // ── Helpers ──

    #[test]
    fn display_helpers_are_stable() {
        assert_eq!(hex_color("#22c55e"), Some(Color::Rgb(0x22, 0xc5, 0x5e)));
        assert_eq!(hex_color("22c55e"), None);
        assert_eq!(hex_color("#22c5"), None);
        assert_eq!(truncate_str("abcdef", 4), "abc…");
        assert_eq!(truncate_str("abc", 4), "abc");
    }

    // ── Render smoke test ──

    #[test]
    fn render_smoke_test_plain_table_layout() {
        let app = app(vec![
            ollama(&["llama3.2:3b", "nomic-embed"]),
            lm_studio(&["qwen2.5-7b"]),
        ]);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        // Heading + filter bar + search placeholder.
        assert!(
            text.contains("PROVIDER FOR CODEX"),
            "missing heading:\n{text}"
        );
        assert!(text.contains("model "), "missing chip row label:\n{text}");
        assert!(text.contains(" all "), "missing the all chip:\n{text}");
        assert!(
            text.contains("search providers…"),
            "missing search placeholder:\n{text}"
        );

        // Two-line rail entries: selected rail + plain rail, titles, and
        // per-entry detail lines.
        assert!(text.contains('┃'), "missing selected rail:\n{text}");
        assert!(text.contains('│'), "missing entry rail:\n{text}");
        assert!(text.contains("Ollama"), "missing provider title:\n{text}");
        assert!(
            text.contains("http://127.0.0.1:11434"),
            "missing provider origin:\n{text}"
        );
        assert!(
            text.contains("llama3.2:3b, nomic-embed"),
            "missing model list line:\n{text}"
        );
        assert!(text.contains("2 models"), "missing model count:\n{text}");
        assert!(text.contains("LM Studio"), "missing second entry:\n{text}");

        // No borders and no sidebar anywhere in the new layout.
        assert!(
            !text.contains('╭') && !text.contains('┌'),
            "no border glyphs should render:\n{text}"
        );
        assert!(
            !text.contains("LOCAL PROVIDERS"),
            "sidebar should be gone:\n{text}"
        );
    }
}
