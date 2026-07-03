//! Dedicated preflight TUI for `pay claude`: pick the local inference provider
//! that Pay should proxy on port 1402, then pick the model Claude Code should
//! request through the Anthropic-compatible environment.

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::commands::server::inference::discovery::DiscoveredProvider;

use super::term::{SPINNER, TuiBackend, with_terminal};
use super::theme::{CARD_BG, SOLANA_GREEN, TOPUP_MAIN_BG, TOPUP_SIDEBAR_BG};
use super::widgets::{controls_bar, sidebar_card, solana_logo};

const PROVIDER_CARD_HEIGHT: u16 = 4;

pub struct ClaudeProviderChoice {
    pub provider: DiscoveredProvider,
    pub model: String,
}

// One short-lived value per TUI invocation — boxing the payload buys nothing.
#[allow(clippy::large_enum_variant)]
pub enum ClaudeProviderSelection {
    Selected(ClaudeProviderChoice),
    Cancelled,
}

pub fn select_claude_provider(
    providers: Vec<DiscoveredProvider>,
    requested_model: Option<&str>,
) -> io::Result<ClaudeProviderSelection> {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) || providers.is_empty() {
        return Ok(ClaudeProviderSelection::Cancelled);
    }

    let requested_model = requested_model.map(str::to_string);
    with_terminal(|terminal| run(terminal, providers, requested_model))
}

fn run(
    terminal: &mut Terminal<TuiBackend>,
    providers: Vec<DiscoveredProvider>,
    requested_model: Option<String>,
) -> io::Result<ClaudeProviderSelection> {
    let mut app = ClaudeProviderApp::new(providers, requested_model);

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
                KeyCode::Up => match app.focus {
                    Focus::Providers => app.move_provider(-1),
                    Focus::Models => app.move_model(-1),
                },
                KeyCode::Down => match app.focus {
                    Focus::Providers => app.move_provider(1),
                    Focus::Models => app.move_model(1),
                },
                KeyCode::Left => app.focus = Focus::Providers,
                KeyCode::Right | KeyCode::Tab => app.focus_models_if_available(),
                KeyCode::BackTab => app.focus = Focus::Providers,
                KeyCode::Home => match app.focus {
                    Focus::Providers => app.set_provider(0),
                    Focus::Models => app.selected_model = 0,
                },
                KeyCode::End => match app.focus {
                    Focus::Providers => app.set_provider(app.providers.len().saturating_sub(1)),
                    Focus::Models => {
                        app.selected_model = app.selected_provider().models.len().saturating_sub(1)
                    }
                },
                KeyCode::Enter => {
                    if let Some(model) = app.selected_model_name() {
                        return Ok(ClaudeProviderSelection::Selected(ClaudeProviderChoice {
                            provider: app.selected_provider().clone(),
                            model,
                        }));
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    return Ok(ClaudeProviderSelection::Cancelled);
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(ClaudeProviderSelection::Cancelled);
                }
                _ => {}
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Providers,
    Models,
}

struct ClaudeProviderApp {
    providers: Vec<DiscoveredProvider>,
    selected_provider: usize,
    selected_model: usize,
    requested_model: Option<String>,
    focus: Focus,
    tick: usize,
}

impl ClaudeProviderApp {
    fn new(providers: Vec<DiscoveredProvider>, requested_model: Option<String>) -> Self {
        Self {
            providers,
            selected_provider: 0,
            selected_model: 0,
            requested_model,
            focus: Focus::Providers,
            tick: 0,
        }
    }

    fn selected_provider(&self) -> &DiscoveredProvider {
        &self.providers[self.selected_provider]
    }

    fn selected_model_name(&self) -> Option<String> {
        self.requested_model.clone().or_else(|| {
            self.selected_provider()
                .models
                .get(self.selected_model)
                .cloned()
        })
    }

    fn model_locked(&self) -> bool {
        self.requested_model.is_some()
    }

    fn focus_models_if_available(&mut self) {
        if !self.model_locked() && !self.selected_provider().models.is_empty() {
            self.focus = Focus::Models;
        }
    }

    fn set_provider(&mut self, index: usize) {
        self.selected_provider = index.min(self.providers.len().saturating_sub(1));
        self.selected_model = self
            .selected_model
            .min(self.selected_provider().models.len().saturating_sub(1));
        if self.selected_provider().models.is_empty() {
            self.selected_model = 0;
            self.focus = Focus::Providers;
        }
    }

    fn move_provider(&mut self, delta: isize) {
        let max = self.providers.len().saturating_sub(1) as isize;
        let next = (self.selected_provider as isize + delta).clamp(0, max) as usize;
        self.set_provider(next);
    }

    fn move_model(&mut self, delta: isize) {
        if self.model_locked() {
            return;
        }
        let max = self.selected_provider().models.len().saturating_sub(1) as isize;
        self.selected_model = (self.selected_model as isize + delta).clamp(0, max) as usize;
    }
}

fn render(frame: &mut ratatui::Frame, app: &ClaudeProviderApp) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );

    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let columns = Layout::horizontal([Constraint::Length(38), Constraint::Min(0)]).split(rows[0]);

    render_provider_sidebar(frame, columns[0], app);
    render_provider_detail(frame, columns[1], app);
    render_controls(frame, rows[1], app);
}

fn render_provider_sidebar(frame: &mut ratatui::Frame, area: Rect, app: &ClaudeProviderApp) {
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_SIDEBAR_BG)),
        area,
    );

    let inner = inset(area, 2, 1);
    let top = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(2),
        Constraint::Min(0),
    ])
    .split(inner);

    frame.render_widget(Paragraph::new(solana_logo("")).centered(), top[0]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "LOCAL PROVIDERS",
            Style::default().fg(Color::DarkGray).bold(),
        ))),
        top[1],
    );

    let mut y = top[2].y;
    let bottom = top[2].bottom();
    for (idx, provider) in app.providers.iter().enumerate() {
        if y + PROVIDER_CARD_HEIGHT > bottom {
            break;
        }

        let selected = idx == app.selected_provider;
        let accent = provider
            .spec
            .color
            .as_deref()
            .and_then(hex_color)
            .unwrap_or(SOLANA_GREEN);
        let title = format!("● {}", provider.spec.title);
        let models = provider.models.len();
        let noun = if models == 1 { "model" } else { "models" };
        let subtitle = format!(
            ":{} · {} {}",
            provider_port(&provider.base_url),
            models,
            noun
        );
        let card_area = Rect {
            x: top[2].x,
            y,
            width: top[2].width,
            height: PROVIDER_CARD_HEIGHT,
        };
        sidebar_card(card_area, frame, &title, &[&subtitle], accent, selected);

        y += PROVIDER_CARD_HEIGHT;
    }
}

fn render_provider_detail(frame: &mut ratatui::Frame, area: Rect, app: &ClaudeProviderApp) {
    frame.render_widget(Block::default().style(Style::default().bg(CARD_BG)), area);

    let provider = app.selected_provider();
    let accent = provider
        .spec
        .color
        .as_deref()
        .and_then(hex_color)
        .unwrap_or(SOLANA_GREEN);
    let block = Block::default()
        .title(Line::from(vec![
            Span::raw(" Claude Code via "),
            Span::styled(
                provider.spec.title.clone(),
                Style::default().fg(accent).bold(),
            ),
            Span::raw(" "),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.focus == Focus::Models {
            Color::Green
        } else {
            Color::DarkGray
        }))
        .style(Style::default().bg(CARD_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = vec![
        Line::default(),
        kv_line("Provider", &provider.spec.title, accent),
        kv_line("Source", &provider.base_url, Color::Gray),
        kv_line("Gateway", "http://127.0.0.1:1402", Color::Gray),
    ];
    if let Some(version) = provider.version.as_deref() {
        lines.push(kv_line("Version", version, Color::Gray));
    }
    lines.push(Line::default());

    if let Some(model) = app.requested_model.as_deref() {
        lines.push(kv_line("Model", model, Color::Green));
        lines.push(Line::from(Span::styled(
            "locked by --model",
            Style::default().fg(Color::DarkGray),
        )));
    } else if provider.models.is_empty() {
        lines.push(Line::from(Span::styled(
            "No models reported by this provider.",
            Style::default().fg(Color::Yellow).bold(),
        )));
        lines.push(Line::from(Span::styled(
            "Pass --model <name> to launch Claude anyway.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "Model",
            Style::default().fg(Color::DarkGray).bold(),
        )));
        for (idx, model) in provider.models.iter().enumerate() {
            let selected = idx == app.selected_model;
            let marker = if selected && app.focus == Focus::Models {
                "▶"
            } else if selected {
                "•"
            } else {
                " "
            };
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled(
                format!(
                    " {marker} {} ",
                    truncate_str(model, inner.width.saturating_sub(6) as usize)
                ),
                style,
            )));
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(Style::default().bg(CARD_BG)),
        inset(inner, 2, 1),
    );
}

fn render_controls(frame: &mut ratatui::Frame, area: Rect, app: &ClaudeProviderApp) {
    let mut entries = vec![("↑ ↓", "select"), ("Enter", "launch"), ("Esc", "cancel")];
    if !app.model_locked() && !app.selected_provider().models.is_empty() {
        entries.insert(1, ("Tab", "provider/model"));
    }

    let spinner = SPINNER[app.tick % SPINNER.len()];
    let status = match app.selected_model_name() {
        Some(model) => Line::from(vec![
            Span::styled(spinner, Style::default().fg(Color::Green).bold()),
            Span::styled(
                format!(" {} / {}", app.selected_provider().spec.slug, model),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        None => Line::from(Span::styled(
            "Pass --model to launch this provider",
            Style::default().fg(Color::Yellow),
        )),
    };
    controls_bar(frame, area, &entries, Some(status));
}

fn kv_line<'a>(label: &'a str, value: &'a str, color: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(Color::DarkGray)),
        Span::styled(value, Style::default().fg(color).bold()),
    ])
}

fn inset(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(x),
        y: area.y.saturating_add(y),
        width: area.width.saturating_sub(x.saturating_mul(2)),
        height: area.height.saturating_sub(y.saturating_mul(2)),
    }
}

fn provider_port(base_url: &str) -> &str {
    base_url
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("?")
}

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

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::server::inference::discovery::{IdentifyProbe, ModelsProbe, ProviderSpec};

    fn provider(models: &[&str]) -> DiscoveredProvider {
        DiscoveredProvider {
            spec: ProviderSpec {
                slug: "ollama".into(),
                title: "Ollama".into(),
                ports: vec![11434],
                identify: vec![IdentifyProbe {
                    path: "/api/version".into(),
                    expect_json_key: Some("version".into()),
                    expect_body_contains: None,
                }],
                models: Some(ModelsProbe {
                    path: "/api/tags".into(),
                    json_pointer: "/models".into(),
                    name_key: "name".into(),
                }),
                color: Some("#22c55e".into()),
                paid: Vec::new(),
            },
            base_url: "http://127.0.0.1:11434".into(),
            models: models.iter().map(|m| (*m).to_string()).collect(),
            version: Some("0.9.1".into()),
        }
    }

    #[test]
    fn requested_model_overrides_provider_models() {
        let app = ClaudeProviderApp::new(
            vec![provider(&["llama3.2", "qwen3.5"])],
            Some("custom-model".to_string()),
        );

        assert_eq!(app.selected_model_name(), Some("custom-model".to_string()));
        assert!(app.model_locked());
    }

    #[test]
    fn provider_model_selection_uses_selected_index() {
        let mut app = ClaudeProviderApp::new(vec![provider(&["llama3.2", "qwen3.5"])], None);
        app.selected_model = 1;

        assert_eq!(app.selected_model_name(), Some("qwen3.5".to_string()));
    }

    #[test]
    fn provider_display_helpers_are_stable() {
        assert_eq!(provider_port("http://127.0.0.1:11434"), "11434");
        assert_eq!(provider_port("not-a-url"), "?");
        assert_eq!(hex_color("#22c55e"), Some(Color::Rgb(0x22, 0xc5, 0x5e)));
        assert_eq!(hex_color("22c55e"), None);
        assert_eq!(truncate_str("abcdef", 4), "abc…");
    }
}
