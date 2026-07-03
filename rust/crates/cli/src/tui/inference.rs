//! Live TUI for `pay serve inference`: provider sidebar, scrolling request
//! table, and per-request detail panel, fed by the PDB event stream
//! (bridged from `broadcast::Sender<SseMessage>` to a `std::sync::mpsc`
//! channel by the caller).
//!
//! Mirrors the topup TUI's visual language — 38-col dark sidebar, content
//! window, 1-row controls bar, rounded borders — and its event-loop shape:
//! 50ms `event::poll` tick, non-blocking channel drain, render, key handling.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use pay_pdb::types::{FlowStatus, PaymentFlow, ProviderSummary, SseMessage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Chart, Clear, Dataset, GraphType, Paragraph,
};

use super::term::{SPINNER, with_terminal};
use super::theme::{
    CARD_BG, SOLANA_GREEN, SOLANA_PURPLE, TOPUP_CARD_BG, TOPUP_MAIN_BG, TOPUP_SIDEBAR_BG,
};
use super::widgets::{controls_bar, sidebar_card, solana_logo};

/// Local flow ring-buffer cap — mirrors PDB's 200-flow ring buffer.
const FLOW_CAP: usize = 200;

/// Maximum per-second rate buckets retained for the live chart; the chart
/// displays the trailing `max(chart width, 60)` seconds of this history.
const CHART_WINDOW: usize = 300;
/// Fixed chart height (1 legend row + plot) at the top of the content pane.
const CHART_HEIGHT: u16 = 10;
/// Hide the chart entirely when the content pane is shorter than this.
const CHART_MIN_PANE_HEIGHT: u16 = 14;

// ── Public API ────────────────────────────────────────────────────────────

/// Everything `run_inference_tui` needs; wired by the `serve inference`
/// command.
pub struct InferenceTuiArgs {
    /// Public gateway URL, e.g. `http://127.0.0.1:1402`.
    pub gateway_url: String,
    /// Web UI URL; `Some` enables the `w` key (opened via `webbrowser`).
    pub web_url: Option<String>,
    /// Providers discovered before the TUI opened.
    pub initial_providers: Vec<ProviderSummary>,
    /// Flows recorded before the TUI opened.
    pub initial_flows: Vec<PaymentFlow>,
    /// Bridged PDB event stream, drained non-blockingly each 50ms tick.
    pub events: Receiver<SseMessage>,
}

/// Runs the TUI on the calling thread; returns when the user quits
/// (q / Ctrl-C / Esc).
pub fn run_inference_tui(args: InferenceTuiArgs) -> io::Result<()> {
    let InferenceTuiArgs {
        gateway_url,
        web_url,
        initial_providers,
        initial_flows,
        events,
    } = args;

    let mut app = InferenceApp::new(initial_providers, initial_flows);

    with_terminal(|terminal| {
        loop {
            app.rates.roll_to(now_unix_secs());
            while let Ok(msg) = events.try_recv() {
                app.apply_event(msg);
            }
            app.tick = app.tick.wrapping_add(1);

            terminal.draw(|frame| render(frame, &app, &gateway_url, web_url.is_some()))?;

            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        match app.handle_key(key.code, key.modifiers) {
                            Action::Quit => return Ok(()),
                            Action::OpenWeb => {
                                if let Some(url) = web_url.as_deref() {
                                    let _ = webbrowser::open(url);
                                }
                            }
                            Action::None => {}
                        }
                    }
                    // Coming back to the tab (or a resize while hidden) can
                    // leave the emulator's copy of the alternate screen out
                    // of sync with ratatui's back-buffer diff — drop the
                    // buffer and repaint everything on the next draw.
                    Event::Resize(_, _) | Event::FocusGained => terminal.clear()?,
                    _ => {}
                }
            }
        }
    })
}

// ── App state ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Providers,
    Requests,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Errors,
    Provider(String),
}

/// What a key press asks the event loop to do beyond mutating state.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Quit,
    OpenWeb,
}

/// Sliding per-second rate buckets feeding the live chart: completion
/// tokens received per second and stablecoin amounts received per second.
/// Oldest bucket at the front, current second at the back.
struct RateHistory {
    tokens: VecDeque<f64>,
    stable: VecDeque<f64>,
    /// Wall-clock second (unix) the back bucket accumulates into.
    current_sec: u64,
}

impl RateHistory {
    fn new(now_sec: u64) -> Self {
        let mut tokens = VecDeque::with_capacity(CHART_WINDOW);
        let mut stable = VecDeque::with_capacity(CHART_WINDOW);
        tokens.push_back(0.0);
        stable.push_back(0.0);
        Self {
            tokens,
            stable,
            current_sec: now_sec,
        }
    }

    /// Slide the window forward to `sec`: push one fresh bucket per elapsed
    /// wall-clock second, dropping buckets beyond [`CHART_WINDOW`].
    fn roll_to(&mut self, sec: u64) {
        if sec <= self.current_sec {
            return;
        }
        let steps = ((sec - self.current_sec) as usize).min(CHART_WINDOW);
        for _ in 0..steps {
            self.tokens.push_back(0.0);
            self.stable.push_back(0.0);
        }
        while self.tokens.len() > CHART_WINDOW {
            self.tokens.pop_front();
            self.stable.pop_front();
        }
        self.current_sec = sec;
    }

    fn add_tokens(&mut self, delta: f64) {
        if let Some(bucket) = self.tokens.back_mut() {
            *bucket += delta;
        }
    }

    fn add_stable(&mut self, amount: f64) {
        if let Some(bucket) = self.stable.back_mut() {
            *bucket += amount;
        }
    }
}

struct InferenceApp {
    providers: Vec<ProviderSummary>,
    /// Flows, newest first, capped at [`FLOW_CAP`].
    flows: VecDeque<PaymentFlow>,
    pane: Pane,
    /// Index into [`Self::up_providers`] — only up providers are rendered
    /// and selectable.
    selected_provider: usize,
    /// Pinned flow id when the user moved the selection; `None` while
    /// following the latest flow.
    selected_flow_id: Option<String>,
    /// True while the newest flow is auto-selected as flows arrive.
    follow: bool,
    filter: Filter,
    /// Render-loop tick (~50ms) driving the spinner glyphs.
    tick: usize,
    /// Per-second token/stablecoin rate buckets for the live chart.
    rates: RateHistory,
    /// Last-seen `tokens_completion` per flow id, so `FlowUpdated` adds
    /// only the positive delta. Pruned alongside the flow ring buffer.
    last_tokens: HashMap<String, u64>,
}

impl InferenceApp {
    fn new(providers: Vec<ProviderSummary>, initial_flows: Vec<PaymentFlow>) -> Self {
        let mut app = Self {
            providers,
            flows: VecDeque::new(),
            pane: Pane::Requests,
            selected_provider: 0,
            selected_flow_id: None,
            follow: true,
            filter: Filter::All,
            tick: 0,
            rates: RateHistory::new(now_unix_secs()),
            last_tokens: HashMap::new(),
        };
        // Initial flows arrive oldest-first (PDB snapshot order); pushing
        // each to the front leaves the newest at the front. Pre-existing
        // token counts are baselines, not new activity — no bucket adds.
        for flow in initial_flows {
            app.seed_token_baseline(&flow);
            app.push_flow(flow);
        }
        app
    }

    // ── Events ──

    fn apply_event(&mut self, msg: SseMessage) {
        match msg {
            SseMessage::Init { .. } => {}
            SseMessage::Snapshot { flows } => {
                self.flows.clear();
                self.last_tokens.clear();
                for flow in flows {
                    // Snapshot totals are history, not fresh activity.
                    self.seed_token_baseline(&flow);
                    self.push_flow(flow);
                }
            }
            SseMessage::FlowCreated { flow } => {
                self.record_flow_activity(&flow, None);
                self.push_flow(flow);
            }
            SseMessage::FlowUpdated { flow } => {
                let prev_status = self
                    .flows
                    .iter()
                    .find(|f| f.id == flow.id)
                    .map(|f| f.status.clone());
                self.record_flow_activity(&flow, prev_status.as_ref());
                match self.flows.iter_mut().find(|f| f.id == flow.id) {
                    Some(existing) => *existing = flow,
                    // Update for a flow we never saw created (e.g. evicted,
                    // or the TUI attached mid-stream) — treat as new.
                    None => self.push_flow(flow),
                }
            }
            SseMessage::ProviderStatus { providers } => {
                self.providers = providers;
                let up_count = self.up_providers().len();
                self.selected_provider = self.selected_provider.min(up_count.saturating_sub(1));
            }
        }
    }

    fn push_flow(&mut self, flow: PaymentFlow) {
        self.flows.push_front(flow);
        while self.flows.len() > FLOW_CAP {
            if let Some(evicted) = self.flows.pop_back() {
                self.last_tokens.remove(&evicted.id);
            }
        }
    }

    // ── Rate accounting (live chart) ──

    /// Record a flow's token count without charting it — used for snapshot
    /// and startup flows whose totals predate the TUI.
    fn seed_token_baseline(&mut self, flow: &PaymentFlow) {
        let tokens = flow
            .inference
            .as_ref()
            .and_then(|info| info.tokens_completion)
            .unwrap_or(0);
        self.last_tokens.insert(flow.id.clone(), tokens);
    }

    /// Chart accounting for fresh flow activity: adds the positive
    /// completion-token delta to the current second's bucket, and — when the
    /// flow transitions to `resource-delivered` with an `amount` — the
    /// parsed stablecoin amount.
    fn record_flow_activity(&mut self, flow: &PaymentFlow, prev_status: Option<&FlowStatus>) {
        let tokens = flow
            .inference
            .as_ref()
            .and_then(|info| info.tokens_completion)
            .unwrap_or(0);
        let last = self.last_tokens.entry(flow.id.clone()).or_insert(0);
        if tokens > *last {
            let delta = tokens - *last;
            *last = tokens;
            self.rates.add_tokens(delta as f64);
        }

        let delivered_now = flow.status == FlowStatus::ResourceDelivered;
        let was_delivered = prev_status.is_some_and(|s| *s == FlowStatus::ResourceDelivered);
        if delivered_now
            && !was_delivered
            && let Some(amount) = flow.amount.as_deref()
            && let Some(value) = parse_stablecoin_amount(amount)
        {
            self.rates.add_stable(value);
        }
    }

    // ── Selection & filtering ──

    /// Providers currently up — the only ones rendered and selectable.
    fn up_providers(&self) -> Vec<&ProviderSummary> {
        self.providers.iter().filter(|p| p.up).collect()
    }

    fn matches_filter(&self, flow: &PaymentFlow) -> bool {
        match &self.filter {
            Filter::All => true,
            Filter::Errors => flow.status == FlowStatus::Failed,
            Filter::Provider(slug) => flow
                .inference
                .as_ref()
                .is_some_and(|info| info.provider == *slug),
        }
    }

    /// Flows passing the active filter, newest first.
    fn filtered_flows(&self) -> Vec<&PaymentFlow> {
        self.flows
            .iter()
            .filter(|flow| self.matches_filter(flow))
            .collect()
    }

    /// Index of the selected flow within [`Self::filtered_flows`].
    fn selected_index(&self) -> Option<usize> {
        let flows = self.filtered_flows();
        if flows.is_empty() {
            return None;
        }
        if self.follow {
            return Some(0);
        }
        self.selected_flow_id
            .as_ref()
            .and_then(|id| flows.iter().position(|flow| flow.id == *id))
            .or(Some(0))
    }

    /// The selected flow (kept for selection tests and future detail
    /// views — the table highlights via [`Self::selected_index`]).
    #[cfg_attr(not(test), allow(dead_code))]
    fn selected_flow(&self) -> Option<&PaymentFlow> {
        let flows = self.filtered_flows();
        self.selected_index()
            .and_then(|idx| flows.get(idx).copied())
    }

    /// Move the request selection by `delta` rows (positive = older).
    /// Any manual move pins the selection and stops following the latest.
    fn move_selection(&mut self, delta: isize) {
        let pinned_id = {
            let flows = self.filtered_flows();
            if flows.is_empty() {
                return;
            }
            let current = self.selected_index().unwrap_or(0) as isize;
            let next = (current + delta).clamp(0, flows.len() as isize - 1) as usize;
            flows[next].id.clone()
        };
        self.selected_flow_id = Some(pinned_id);
        self.follow = false;
    }

    fn toggle_follow(&mut self) {
        if self.follow {
            // Pin whatever is currently newest.
            let pinned = self.filtered_flows().first().map(|flow| flow.id.clone());
            self.follow = false;
            self.selected_flow_id = pinned;
        } else {
            self.follow = true;
            self.selected_flow_id = None;
        }
    }

    /// Cycle All → Errors → each up provider in sidebar order → All.
    fn cycle_filter(&mut self) {
        self.filter = match &self.filter {
            Filter::All => Filter::Errors,
            Filter::Errors => match self.up_providers().first() {
                Some(p) => Filter::Provider(p.slug.clone()),
                None => Filter::All,
            },
            Filter::Provider(slug) => {
                let ups = self.up_providers();
                let next = ups
                    .iter()
                    .position(|p| p.slug == *slug)
                    .and_then(|idx| ups.get(idx + 1));
                match next {
                    Some(p) => Filter::Provider(p.slug.clone()),
                    None => Filter::All,
                }
            }
        };
        // If the pinned flow got filtered out, fall back to following.
        let pinned_visible = self
            .selected_flow_id
            .as_ref()
            .is_some_and(|id| self.filtered_flows().iter().any(|flow| flow.id == *id));
        if !pinned_visible {
            self.selected_flow_id = None;
            self.follow = true;
        }
    }

    fn filter_label(&self) -> String {
        match &self.filter {
            Filter::All => "all".to_string(),
            Filter::Errors => "errors".to_string(),
            Filter::Provider(slug) => slug.clone(),
        }
    }

    /// Clear the local flow list (display only — PDB history is untouched;
    /// chart history is kept, token baselines reset with the list).
    fn clear_flows(&mut self) {
        self.flows.clear();
        self.last_tokens.clear();
        self.selected_flow_id = None;
        self.follow = true;
    }

    // ── Keys ──

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Action {
        match code {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                return Action::Quit;
            }
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Tab | KeyCode::BackTab => {
                self.pane = match self.pane {
                    Pane::Providers => Pane::Requests,
                    Pane::Requests => Pane::Providers,
                };
            }
            KeyCode::Up => match self.pane {
                Pane::Providers => {
                    self.selected_provider = self.selected_provider.saturating_sub(1);
                }
                Pane::Requests => self.move_selection(-1),
            },
            KeyCode::Down => match self.pane {
                Pane::Providers => {
                    if self.selected_provider + 1 < self.up_providers().len() {
                        self.selected_provider += 1;
                    }
                }
                Pane::Requests => self.move_selection(1),
            },
            KeyCode::Enter => self.toggle_follow(),
            KeyCode::Char('f') | KeyCode::Char('F') => self.cycle_filter(),
            KeyCode::Char('c') | KeyCode::Char('C') => self.clear_flows(),
            KeyCode::Char('w') | KeyCode::Char('W') => return Action::OpenWeb,
            _ => {}
        }
        Action::None
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────

fn render(frame: &mut Frame, app: &InferenceApp, gateway_url: &str, has_web: bool) {
    let area = frame.area();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );

    let rows = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);

    let columns = Layout::horizontal([Constraint::Length(38), Constraint::Min(32)]).split(rows[0]);
    render_providers(frame, columns[0], app);
    render_content(frame, columns[1], app);

    render_controls(frame, rows[1], app, gateway_url, has_web);
}

/// Height of a provider card: blank, title, subtitle, trailing blank.
const PROVIDER_CARD_HEIGHT: u16 = 4;

/// Left pane: Solana logo, then one card per **up** provider, plus the
/// selected provider's models. Down providers are not rendered.
fn render_providers(frame: &mut Frame, area: Rect, app: &InferenceApp) {
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_SIDEBAR_BG)),
        area,
    );

    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let bottom = inner.y + inner.height;
    let mut y = inner.y;

    // Solana logo at the top of the sidebar, like the topup TUI.
    let logo = solana_logo("");
    let logo_height = logo.len() as u16;
    if y + logo_height <= bottom {
        frame.render_widget(
            Paragraph::new(logo).centered(),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: logo_height,
            },
        );
    }
    y += logo_height + 1;

    if y < bottom {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "PROVIDERS",
                Style::default().fg(Color::DarkGray).bold(),
            ))),
            Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            },
        );
    }
    y += 2;

    let providers = app.up_providers();
    if providers.is_empty() {
        if y < bottom {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no providers detected",
                    Style::default().fg(Color::DarkGray),
                ))),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
        }
        return;
    }

    for (idx, provider) in providers.iter().enumerate() {
        if y + PROVIDER_CARD_HEIGHT > bottom {
            break;
        }
        let selected = app.pane == Pane::Providers && idx == app.selected_provider;
        let accent = provider
            .color
            .as_deref()
            .and_then(hex_color)
            .unwrap_or(SOLANA_GREEN);
        let title = format!("● {}", provider.title);
        let models = provider.models.len();
        let noun = if models == 1 { "model" } else { "models" };
        let subtitle = format!(
            ":{} · {} {}",
            provider_port(&provider.base_url),
            models,
            noun
        );
        let card_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: PROVIDER_CARD_HEIGHT,
        };
        sidebar_card(card_area, frame, &title, &[&subtitle], accent, selected);

        // Overdraw the status dot in the provider's brand color —
        // sidebar_card styles the whole title uniformly.
        if !selected {
            let title_width = title.chars().count() as u16;
            let dot_x = inner.x + inner.width.saturating_sub(title_width) / 2;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "●",
                    Style::default().fg(accent).bg(TOPUP_CARD_BG),
                )),
                Rect {
                    x: dot_x.min(inner.x + inner.width.saturating_sub(1)),
                    y: y + 1,
                    width: 1,
                    height: 1,
                },
            );
        }
        y += PROVIDER_CARD_HEIGHT;

        if idx == app.selected_provider {
            for model in &provider.models {
                if y >= bottom {
                    break;
                }
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!(
                            " ▸ {}",
                            truncate_str(model, inner.width.saturating_sub(3).into())
                        ),
                        Style::default().fg(Color::DarkGray),
                    ))),
                    Rect {
                        x: inner.x,
                        y,
                        width: inner.width,
                        height: 1,
                    },
                );
                y += 1;
            }
        }
        y += 1; // gap between providers
    }
}

fn render_content(frame: &mut Frame, area: Rect, app: &InferenceApp) {
    // Chart on top (fixed height), hidden when the pane is too small; the
    // request table fills all remaining height.
    let chart_height = if area.height < CHART_MIN_PANE_HEIGHT {
        0
    } else {
        CHART_HEIGHT
    };
    let split =
        Layout::vertical([Constraint::Length(chart_height), Constraint::Min(0)]).split(area);
    if chart_height > 0 {
        render_chart(frame, split[0], app);
    }
    render_requests(frame, split[1], app);
}

/// scope-tui-style live chart: braille line plot of completion tokens/s
/// (purple) and stablecoins/s (green) over a sliding per-second window.
/// No borders, no axis labels — one legend row with the window peaks, each
/// series normalized to its own max.
fn render_chart(frame: &mut Frame, area: Rect, app: &InferenceApp) {
    if area.height < 2 || area.width == 0 {
        return;
    }
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    let plot = rows[1];

    // Window = as many seconds as the plot is wide, at least 60.
    let window = (plot.width as usize).max(60);
    let (token_points, token_peak) = series_points(&app.rates.tokens, window);
    let (stable_points, stable_peak) = series_points(&app.rates.stable, window);

    let legend = Line::from(vec![
        Span::styled("tok/s ▮ ", Style::default().fg(SOLANA_PURPLE)),
        Span::styled(
            format!("{token_peak:.0}"),
            Style::default().fg(SOLANA_PURPLE).bold(),
        ),
        Span::raw("   "),
        Span::styled("usdc/s ▮ ", Style::default().fg(SOLANA_GREEN)),
        Span::styled(
            format!("{stable_peak:.2}"),
            Style::default().fg(SOLANA_GREEN).bold(),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(legend).style(Style::default().bg(TOPUP_MAIN_BG)),
        rows[0],
    );

    let datasets = vec![
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(SOLANA_PURPLE))
            .data(&token_points),
        Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(SOLANA_GREEN))
            .data(&stable_points),
    ];
    let chart = Chart::new(datasets)
        .x_axis(Axis::default().bounds([0.0, (window.saturating_sub(1)) as f64]))
        // Series are normalized to their own peaks; small headroom keeps
        // the max off the top edge.
        .y_axis(Axis::default().bounds([0.0, 1.05]))
        .style(Style::default().bg(TOPUP_MAIN_BG));
    frame.render_widget(chart, plot);
}

/// Trailing `window` buckets as chart points normalized to the window's
/// peak (each series scales to its own max); returns the points and the
/// peak. Data shorter than the window is right-aligned (newest at the
/// right edge).
fn series_points(buckets: &VecDeque<f64>, window: usize) -> (Vec<(f64, f64)>, f64) {
    let len = buckets.len().min(window);
    let skip = buckets.len() - len;
    let peak = buckets.iter().skip(skip).copied().fold(0.0, f64::max);
    let denom = if peak > 0.0 { peak } else { 1.0 };
    let start_x = window - len;
    let points = buckets
        .iter()
        .skip(skip)
        .enumerate()
        .map(|(i, value)| ((start_x + i) as f64, value / denom))
        .collect();
    (points, peak)
}

// Request-table column widths (characters).
const COL_TIME: usize = 10;
const COL_PROV: usize = 9;
const COL_MODEL: usize = 15;
const COL_STATUS: usize = 5;
const COL_TOKS: usize = 7;
/// Selection marker column ("▸ ").
const COL_MARKER: usize = 2;

fn render_requests(frame: &mut Frame, area: Rect, app: &InferenceApp) {
    let flows = app.filtered_flows();
    let mut title = format!(" REQUESTS · {} total ", app.flows.len());
    if app.filter != Filter::All {
        title = format!(
            " REQUESTS · {} total · filter {} ",
            app.flows.len(),
            app.filter_label()
        );
    }
    let border_color = if app.pane == Pane::Requests {
        Color::Green
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(Color::White).bold(),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(TOPUP_MAIN_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let width = inner.width as usize;
    let path_width = width
        .saturating_sub(COL_MARKER + COL_TIME + COL_PROV + COL_MODEL + COL_STATUS + COL_TOKS)
        .max(4);

    // Header row.
    let header = format!(
        "{marker}{time:<tw$}{prov:<pw$}{model:<mw$}{path:<paw$}{st:<sw$}{tok:>tkw$}",
        marker = " ".repeat(COL_MARKER),
        time = "time",
        prov = "prov",
        model = "model",
        path = "path",
        st = "st",
        tok = "tok/s",
        tw = COL_TIME,
        pw = COL_PROV,
        mw = COL_MODEL,
        paw = path_width,
        sw = COL_STATUS,
        tkw = COL_TOKS,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            header,
            Style::default().fg(Color::DarkGray),
        ))),
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        },
    );

    if flows.is_empty() {
        frame.render_widget(
            Paragraph::new(
                Line::from(Span::styled(
                    "waiting for requests…",
                    Style::default().fg(Color::DarkGray),
                ))
                .centered(),
            ),
            Rect {
                x: inner.x,
                y: inner.y + inner.height / 2,
                width: inner.width,
                height: 1,
            },
        );
        return;
    }

    // Scroll the window so the selected row stays visible.
    let visible = inner.height.saturating_sub(1) as usize;
    if visible == 0 {
        return;
    }
    let selected = app.selected_index().unwrap_or(0);
    let offset = selected.saturating_sub(visible.saturating_sub(1));

    for (row, (idx, flow)) in flows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let line = flow_row(app, flow, path_width, idx == selected);
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: inner.x,
                y: inner.y + 1 + row as u16,
                width: inner.width,
                height: 1,
            },
        );
    }
}

/// One request-table row: time, provider, model, path, status, tok/s.
fn flow_row<'a>(
    app: &InferenceApp,
    flow: &'a PaymentFlow,
    path_width: usize,
    selected: bool,
) -> Line<'a> {
    let provider = flow
        .inference
        .as_ref()
        .map(|info| info.provider.as_str())
        .unwrap_or("—");
    let model = flow
        .inference
        .as_ref()
        .and_then(|info| info.model.as_deref())
        .unwrap_or("—");
    let (status, status_color) = status_cell(flow, app.tick);

    let marker = if selected { "▸ " } else { "  " };
    let mut spans = vec![
        Span::styled(marker, Style::default().fg(SOLANA_GREEN).bold()),
        Span::styled(
            pad(&short_time(&flow.started_at), COL_TIME),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            pad(&truncate_str(provider, COL_PROV - 1), COL_PROV),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            pad(&truncate_str(model, COL_MODEL - 1), COL_MODEL),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            pad(
                &truncate_str(&flow.resource, path_width.saturating_sub(1)),
                path_width,
            ),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            pad(&status, COL_STATUS),
            Style::default().fg(status_color).bold(),
        ),
        Span::styled(
            format!("{:>width$}", tokens_per_sec_label(flow), width = COL_TOKS),
            Style::default().fg(Color::White),
        ),
    ];
    if selected {
        spans = spans
            .into_iter()
            .map(|span| {
                let style = span.style.bg(CARD_BG);
                span.style(style)
            })
            .collect();
    }
    Line::from(spans)
}

fn render_controls(
    frame: &mut Frame,
    area: Rect,
    app: &InferenceApp,
    gateway_url: &str,
    has_web: bool,
) {
    let follow_label = if app.follow {
        "following"
    } else {
        "follow latest"
    };
    let filter_label = format!("filter: {}", app.filter_label());
    let mut entries: Vec<(&str, &str)> = vec![
        ("↑ ↓", "select"),
        ("⇥", "pane"),
        ("⏎", follow_label),
        ("f", &filter_label),
        ("c", "clear"),
    ];
    if has_web {
        entries.push(("w", "web ui"));
    }
    entries.push(("q", "quit"));

    let status = Line::from(vec![
        Span::styled(
            SPINNER[app.tick % SPINNER.len()].to_string(),
            Style::default().fg(SOLANA_GREEN).bold(),
        ),
        Span::styled(" live", Style::default().fg(SOLANA_GREEN)),
        Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{gateway_url} "), Style::default().fg(Color::Cyan)),
    ]);
    controls_bar(frame, area, &entries, Some(status));
}

// ── Display helpers ───────────────────────────────────────────────────────

/// Table status cell: spinner while in-progress, status code (or ✓) when
/// done, red on failure.
fn status_cell(flow: &PaymentFlow, tick: usize) -> (String, Color) {
    match flow.status {
        FlowStatus::InProgress => (SPINNER[tick % SPINNER.len()].to_string(), Color::Yellow),
        FlowStatus::Failed => (
            flow_status_code(flow).map_or_else(|| "✗".to_string(), |c| c.to_string()),
            Color::Red,
        ),
        _ => (
            flow_status_code(flow).map_or_else(|| "✓".to_string(), |c| c.to_string()),
            Color::Green,
        ),
    }
}

/// Extract the upstream HTTP status code from the completion event that
/// `AllExchanges` correlation appends (`"200 — completed in 12ms"`).
fn flow_status_code(flow: &PaymentFlow) -> Option<u16> {
    flow.events.iter().rev().find_map(|event| {
        let (code, rest) = event.message.split_once(' ')?;
        (rest.starts_with("— completed") && code.len() == 3)
            .then(|| code.parse().ok())
            .flatten()
    })
}

fn tokens_per_sec_label(flow: &PaymentFlow) -> String {
    flow.inference
        .as_ref()
        .and_then(|info| info.tokens_per_sec)
        .map_or_else(|| "—".to_string(), |t| format!("{t:.1}"))
}

/// RFC3339 timestamp → local `HH:MM:SS`.
fn short_time(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| ts.get(11..19).unwrap_or(ts).to_string())
}

/// Current wall-clock time in whole unix seconds.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse the leading decimal of a flow amount (`"0.0100 USDC"` → `0.01`).
/// Non-numeric amounts (e.g. `"unbounded"`) yield `None`.
fn parse_stablecoin_amount(amount: &str) -> Option<f64> {
    let token = amount.split_whitespace().next()?;
    let value: f64 = token.parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// Trailing port digits of a base URL (`http://127.0.0.1:11434` → `11434`).
fn provider_port(base_url: &str) -> &str {
    base_url
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or("?")
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

/// Left-pad/pad `s` to exactly `width` characters.
fn pad(s: &str, width: usize) -> String {
    format!("{s:<width$}")
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use pay_pdb::types::{FlowEvent, InferenceInfo, Protocol};

    fn provider(slug: &str, title: &str, up: bool, models: &[&str]) -> ProviderSummary {
        ProviderSummary {
            slug: slug.to_string(),
            title: title.to_string(),
            base_url: "http://127.0.0.1:11434".to_string(),
            up,
            models: models.iter().map(|m| m.to_string()).collect(),
            version: None,
            color: Some("#22c55e".to_string()),
        }
    }

    fn flow(id: &str, status: FlowStatus, provider: Option<&str>) -> PaymentFlow {
        PaymentFlow {
            id: id.to_string(),
            protocol: Protocol::Http,
            scheme: None,
            resource: "/v1/chat/completions".to_string(),
            status,
            client_ip: "::1".to_string(),
            started_at: "2026-07-01T12:01:22.101Z".to_string(),
            updated_at: "2026-07-01T12:01:24.020Z".to_string(),
            duration_ms: 1919,
            amount: None,
            steps: vec![],
            events: vec![
                FlowEvent {
                    ts: "2026-07-01T12:01:22.101Z".to_string(),
                    message: "POST /v1/chat/completions".to_string(),
                    detail: Some("Request forwarded upstream".to_string()),
                },
                FlowEvent {
                    ts: "2026-07-01T12:01:24.020Z".to_string(),
                    message: "200 — completed in 1919ms".to_string(),
                    detail: None,
                },
            ],
            challenge_headers: None,
            payer: None,
            session: None,
            payment_headers: None,
            response_headers: None,
            response_body: None,
            inference: provider.map(|slug| InferenceInfo {
                provider: slug.to_string(),
                model: Some("llama3.2:3b".to_string()),
                endpoint_kind: Some("chat".to_string()),
                streamed: true,
                tokens_prompt: Some(12),
                tokens_completion: Some(214),
                ttft_ms: Some(182),
                tokens_per_sec: Some(41.2),
            }),
        }
    }

    fn app_with(providers: Vec<ProviderSummary>, flows: Vec<PaymentFlow>) -> InferenceApp {
        InferenceApp::new(providers, flows)
    }

    // ── Event application ──

    #[test]
    fn snapshot_replaces_flows() {
        let mut app = app_with(
            vec![],
            vec![flow("old", FlowStatus::ResourceDelivered, None)],
        );
        app.apply_event(SseMessage::Snapshot {
            flows: vec![
                flow("a", FlowStatus::ResourceDelivered, None),
                flow("b", FlowStatus::InProgress, None),
            ],
        });
        // Snapshot arrives oldest-first; newest ("b") ends up at the front.
        let ids: Vec<&str> = app.flows.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn flow_created_pushes_newest_first_and_ring_buffer_caps_at_200() {
        let mut app = app_with(vec![], vec![]);
        for i in 0..205 {
            app.apply_event(SseMessage::FlowCreated {
                flow: flow(&format!("flow-{i}"), FlowStatus::InProgress, None),
            });
        }
        assert_eq!(app.flows.len(), FLOW_CAP);
        assert_eq!(app.flows.front().unwrap().id, "flow-204");
        assert_eq!(app.flows.back().unwrap().id, "flow-5");
    }

    #[test]
    fn flow_updated_replaces_by_id_and_falls_back_to_push() {
        let mut app = app_with(vec![], vec![flow("a", FlowStatus::InProgress, None)]);
        app.apply_event(SseMessage::FlowUpdated {
            flow: flow("a", FlowStatus::ResourceDelivered, Some("ollama")),
        });
        assert_eq!(app.flows.len(), 1);
        assert_eq!(app.flows[0].status, FlowStatus::ResourceDelivered);
        assert!(app.flows[0].inference.is_some());

        // Unknown id (evicted or missed create) is treated as a new flow.
        app.apply_event(SseMessage::FlowUpdated {
            flow: flow("never-seen", FlowStatus::Failed, None),
        });
        assert_eq!(app.flows.len(), 2);
        assert_eq!(app.flows.front().unwrap().id, "never-seen");
    }

    #[test]
    fn provider_status_replaces_providers_and_clamps_selection_to_up_providers() {
        let mut app = app_with(
            vec![
                provider("ollama", "Ollama", true, &[]),
                provider("vllm", "vLLM", true, &[]),
            ],
            vec![],
        );
        assert_eq!(app.up_providers().len(), 2);
        app.selected_provider = 1;

        // ollama goes down: only vllm remains selectable, selection clamps.
        app.apply_event(SseMessage::ProviderStatus {
            providers: vec![
                provider("ollama", "Ollama", false, &[]),
                provider("vllm", "vLLM", true, &[]),
            ],
        });
        assert_eq!(app.providers.len(), 2);
        let ups: Vec<&str> = app.up_providers().iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(ups, vec!["vllm"]);
        assert_eq!(app.selected_provider, 0);

        // Everything down: selection clamps to 0, up list is empty.
        app.apply_event(SseMessage::ProviderStatus {
            providers: vec![provider("ollama", "Ollama", false, &[])],
        });
        assert!(app.up_providers().is_empty());
        assert_eq!(app.selected_provider, 0);
    }

    #[test]
    fn init_message_is_ignored() {
        let mut app = app_with(vec![], vec![flow("a", FlowStatus::InProgress, None)]);
        app.apply_event(SseMessage::Init {
            viewer_ip: "::1".into(),
        });
        assert_eq!(app.flows.len(), 1);
        assert!(app.providers.is_empty());
    }

    // ── Follow-latest ──

    #[test]
    fn follow_latest_tracks_new_flows_until_user_moves_selection() {
        let mut app = app_with(
            vec![],
            vec![flow("f1", FlowStatus::ResourceDelivered, None)],
        );
        assert!(app.follow);
        assert_eq!(app.selected_flow().unwrap().id, "f1");

        // While following, a new flow moves the selection.
        app.apply_event(SseMessage::FlowCreated {
            flow: flow("f2", FlowStatus::InProgress, None),
        });
        assert_eq!(app.selected_flow().unwrap().id, "f2");

        // Moving the selection pins it…
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert!(!app.follow);
        assert_eq!(app.selected_flow().unwrap().id, "f1");

        // …so new flows no longer steal it.
        app.apply_event(SseMessage::FlowCreated {
            flow: flow("f3", FlowStatus::InProgress, None),
        });
        assert_eq!(app.selected_flow().unwrap().id, "f1");
    }

    #[test]
    fn enter_toggles_follow_latest() {
        let mut app = app_with(
            vec![],
            vec![
                flow("f1", FlowStatus::ResourceDelivered, None),
                flow("f2", FlowStatus::ResourceDelivered, None),
            ],
        );
        // Pin, then re-follow via Enter.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert!(!app.follow);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.follow);
        assert_eq!(app.selected_flow().unwrap().id, "f2"); // newest

        // Enter while following pins the current newest.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.follow);
        assert_eq!(app.selected_flow_id.as_deref(), Some("f2"));
    }

    // ── Filtering ──

    #[test]
    fn filter_cycles_all_errors_then_up_providers_only() {
        let mut app = app_with(
            vec![
                provider("ollama", "Ollama", true, &[]),
                provider("llama-cpp", "llama.cpp", false, &[]),
                provider("lm-studio", "LM Studio", true, &[]),
            ],
            vec![],
        );
        assert_eq!(app.filter, Filter::All);
        app.cycle_filter();
        assert_eq!(app.filter, Filter::Errors);
        app.cycle_filter();
        assert_eq!(app.filter, Filter::Provider("ollama".into()));
        // llama-cpp is down — skipped.
        app.cycle_filter();
        assert_eq!(app.filter, Filter::Provider("lm-studio".into()));
        app.cycle_filter();
        assert_eq!(app.filter, Filter::All);
    }

    #[test]
    fn filter_cycle_without_up_providers_skips_provider_stage() {
        let mut app = app_with(vec![provider("ollama", "Ollama", false, &[])], vec![]);
        app.cycle_filter();
        assert_eq!(app.filter, Filter::Errors);
        app.cycle_filter();
        assert_eq!(app.filter, Filter::All);
    }

    #[test]
    fn errors_and_provider_filters_narrow_the_flow_list() {
        let mut app = app_with(
            vec![provider("ollama", "Ollama", true, &[])],
            vec![
                flow("ok", FlowStatus::ResourceDelivered, Some("ollama")),
                flow("bad", FlowStatus::Failed, None),
            ],
        );
        assert_eq!(app.filtered_flows().len(), 2);

        app.filter = Filter::Errors;
        let ids: Vec<&str> = app.filtered_flows().iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["bad"]);

        app.filter = Filter::Provider("ollama".into());
        let ids: Vec<&str> = app.filtered_flows().iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["ok"]);
    }

    #[test]
    fn cycling_filter_releases_pinned_flow_that_gets_filtered_out() {
        let mut app = app_with(
            vec![],
            vec![
                flow("ok", FlowStatus::ResourceDelivered, None),
                flow("bad", FlowStatus::Failed, None),
            ],
        );
        // Pin the delivered flow, then switch to the errors filter.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected_flow_id.as_deref(), Some("ok"));
        app.cycle_filter();
        assert_eq!(app.filter, Filter::Errors);
        assert!(app.follow);
        assert_eq!(app.selected_flow().unwrap().id, "bad");
    }

    // ── Keys ──

    #[test]
    fn tab_toggles_pane_and_arrows_move_within_up_providers() {
        let mut app = app_with(
            vec![
                provider("ollama", "Ollama", true, &[]),
                provider("vllm", "vLLM", true, &[]),
                provider("llama-cpp", "llama.cpp", false, &[]),
            ],
            vec![],
        );
        assert_eq!(app.pane, Pane::Requests);
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.pane, Pane::Providers);

        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected_provider, 1);
        // Clamped to the 2 up providers — the down one is not selectable.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected_provider, 1);
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected_provider, 0);

        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.pane, Pane::Requests);
    }

    #[test]
    fn clear_empties_flow_list_and_resumes_follow() {
        let mut app = app_with(
            vec![],
            vec![
                flow("f1", FlowStatus::ResourceDelivered, None),
                flow("f2", FlowStatus::ResourceDelivered, None),
            ],
        );
        app.handle_key(KeyCode::Down, KeyModifiers::NONE); // pin
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.flows.is_empty());
        assert!(app.follow);
        assert!(app.selected_flow_id.is_none());
    }

    #[test]
    fn quit_and_web_keys_produce_actions() {
        let mut app = app_with(vec![], vec![]);
        assert_eq!(
            app.handle_key(KeyCode::Char('q'), KeyModifiers::NONE),
            Action::Quit
        );
        assert_eq!(
            app.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            Action::Quit
        );
        assert_eq!(
            app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Action::Quit
        );
        assert_eq!(
            app.handle_key(KeyCode::Char('w'), KeyModifiers::NONE),
            Action::OpenWeb
        );
        assert_eq!(
            app.handle_key(KeyCode::Char('x'), KeyModifiers::NONE),
            Action::None
        );
    }

    // ── Chart rate accounting ──

    /// Flow with a specific completion-token count.
    fn flow_tokens(id: &str, status: FlowStatus, tokens: u64) -> PaymentFlow {
        let mut f = flow(id, status, Some("ollama"));
        f.inference.as_mut().unwrap().tokens_completion = Some(tokens);
        f
    }

    #[test]
    fn token_deltas_accumulate_into_current_bucket() {
        let mut app = app_with(vec![], vec![]);

        // FlowCreated with an initial count charts the full count.
        app.apply_event(SseMessage::FlowCreated {
            flow: flow_tokens("a", FlowStatus::InProgress, 100),
        });
        assert_eq!(app.rates.tokens.back(), Some(&100.0));

        // FlowUpdated on the same flow charts only the positive delta.
        app.apply_event(SseMessage::FlowUpdated {
            flow: flow_tokens("a", FlowStatus::InProgress, 160),
        });
        assert_eq!(app.rates.tokens.back(), Some(&160.0));

        // A lower/equal count charts nothing.
        app.apply_event(SseMessage::FlowUpdated {
            flow: flow_tokens("a", FlowStatus::InProgress, 150),
        });
        assert_eq!(app.rates.tokens.back(), Some(&160.0));

        // A second flow adds independently.
        app.apply_event(SseMessage::FlowCreated {
            flow: flow_tokens("b", FlowStatus::InProgress, 40),
        });
        assert_eq!(app.rates.tokens.back(), Some(&200.0));
    }

    #[test]
    fn snapshot_and_initial_flows_seed_baselines_without_charting() {
        // Initial flows: totals predate the TUI, nothing charted.
        let mut app = app_with(vec![], vec![flow_tokens("a", FlowStatus::InProgress, 500)]);
        assert!(app.rates.tokens.iter().all(|v| *v == 0.0));

        // Snapshot: same — but a later update charts only the delta.
        app.apply_event(SseMessage::Snapshot {
            flows: vec![flow_tokens("b", FlowStatus::InProgress, 300)],
        });
        assert!(app.rates.tokens.iter().all(|v| *v == 0.0));
        app.apply_event(SseMessage::FlowUpdated {
            flow: flow_tokens("b", FlowStatus::InProgress, 336),
        });
        assert_eq!(app.rates.tokens.back(), Some(&36.0));
    }

    #[test]
    fn bucket_window_slides_and_caps_at_chart_window() {
        let mut app = app_with(vec![], vec![]);
        let start = app.rates.current_sec;
        app.rates.add_tokens(5.0);

        // One elapsed second: previous bucket keeps its total, fresh
        // current bucket starts at zero.
        app.rates.roll_to(start + 1);
        assert_eq!(app.rates.tokens.len(), 2);
        assert_eq!(app.rates.tokens[0], 5.0);
        assert_eq!(app.rates.tokens.back(), Some(&0.0));

        // Time going backwards (or repeating) is a no-op.
        app.rates.roll_to(start);
        assert_eq!(app.rates.tokens.len(), 2);

        // A large jump caps the history at the window size.
        app.rates.roll_to(start + 10_000);
        assert_eq!(app.rates.tokens.len(), CHART_WINDOW);
        assert_eq!(app.rates.stable.len(), CHART_WINDOW);
        assert!(app.rates.tokens.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn evicted_flows_prune_token_baselines() {
        let mut app = app_with(vec![], vec![]);
        for i in 0..(FLOW_CAP + 5) {
            app.apply_event(SseMessage::FlowCreated {
                flow: flow_tokens(&format!("flow-{i}"), FlowStatus::InProgress, 10),
            });
        }
        assert_eq!(app.last_tokens.len(), FLOW_CAP);
        assert!(!app.last_tokens.contains_key("flow-0"));
        assert!(
            app.last_tokens
                .contains_key(&format!("flow-{}", FLOW_CAP + 4))
        );
    }

    #[test]
    fn stablecoin_amounts_chart_on_delivery_transition_only() {
        let mut app = app_with(vec![], vec![]);
        let mut f = flow("a", FlowStatus::InProgress, None);
        f.amount = Some("0.0100 USDC".to_string());

        // In-progress with an amount: nothing charted yet.
        app.apply_event(SseMessage::FlowCreated { flow: f.clone() });
        assert_eq!(app.rates.stable.back(), Some(&0.0));

        // Transition to delivered charts the amount…
        f.status = FlowStatus::ResourceDelivered;
        app.apply_event(SseMessage::FlowUpdated { flow: f.clone() });
        assert_eq!(app.rates.stable.back(), Some(&0.01));

        // …and a repeat update while already delivered does not double-count.
        app.apply_event(SseMessage::FlowUpdated { flow: f.clone() });
        assert_eq!(app.rates.stable.back(), Some(&0.01));

        // A flow created already-delivered (one-shot exchange) counts once.
        let mut g = flow("b", FlowStatus::ResourceDelivered, None);
        g.amount = Some("0.0200 USDC".to_string());
        app.apply_event(SseMessage::FlowCreated { flow: g });
        assert!((app.rates.stable.back().unwrap() - 0.03).abs() < 1e-9);

        // Unparseable amounts are ignored.
        let mut h = flow("c", FlowStatus::ResourceDelivered, None);
        h.amount = Some("unbounded".to_string());
        app.apply_event(SseMessage::FlowCreated { flow: h });
        assert!((app.rates.stable.back().unwrap() - 0.03).abs() < 1e-9);
    }

    #[test]
    fn parse_stablecoin_amount_handles_amount_formats() {
        assert_eq!(parse_stablecoin_amount("0.0100 USDC"), Some(0.01));
        assert_eq!(parse_stablecoin_amount("12.5"), Some(12.5));
        assert_eq!(parse_stablecoin_amount("unbounded"), None);
        assert_eq!(parse_stablecoin_amount(""), None);
        assert_eq!(parse_stablecoin_amount("-1 USDC"), None);
        assert_eq!(parse_stablecoin_amount("NaN USDC"), None);
    }

    #[test]
    fn series_points_normalize_and_right_align() {
        let mut buckets = VecDeque::new();
        buckets.extend([0.0, 5.0, 10.0]);
        let (points, peak) = series_points(&buckets, 60);
        assert_eq!(peak, 10.0);
        assert_eq!(points.len(), 3);
        // Right-aligned: newest bucket lands at x = window - 1.
        assert_eq!(points[0], (57.0, 0.0));
        assert_eq!(points[2], (59.0, 1.0));
        assert_eq!(points[1], (58.0, 0.5));

        // All-zero series: peak 0, values stay 0 (no divide-by-zero).
        let zeros: VecDeque<f64> = [0.0, 0.0].into_iter().collect();
        let (points, peak) = series_points(&zeros, 60);
        assert_eq!(peak, 0.0);
        assert!(points.iter().all(|(_, y)| *y == 0.0));
    }

    // ── Display helpers ──

    #[test]
    fn status_helpers_extract_code_and_tok_s() {
        let done = flow("a", FlowStatus::ResourceDelivered, Some("ollama"));
        assert_eq!(flow_status_code(&done), Some(200));
        assert_eq!(tokens_per_sec_label(&done), "41.2");
        assert_eq!(status_cell(&done, 0), ("200".to_string(), Color::Green));

        let mut failed = flow("b", FlowStatus::Failed, None);
        failed.events.clear();
        assert_eq!(status_cell(&failed, 0), ("✗".to_string(), Color::Red));
        assert_eq!(tokens_per_sec_label(&failed), "—");

        let in_progress = flow("c", FlowStatus::InProgress, None);
        let (glyph, color) = status_cell(&in_progress, 3);
        assert_eq!(glyph, SPINNER[3]);
        assert_eq!(color, Color::Yellow);
    }

    #[test]
    fn misc_helpers_format_ports_colors_and_truncation() {
        assert_eq!(provider_port("http://127.0.0.1:11434"), "11434");
        assert_eq!(provider_port("http://127.0.0.1:1234/"), "1234");
        assert_eq!(provider_port("nonsense"), "?");
        assert_eq!(hex_color("#22c55e"), Some(Color::Rgb(0x22, 0xc5, 0x5e)));
        assert_eq!(hex_color("22c55e"), None);
        assert_eq!(hex_color("#22c5"), None);
        assert_eq!(truncate_str("abcdef", 4), "abc…");
        assert_eq!(truncate_str("abc", 4), "abc");
        assert_eq!(pad("ab", 4), "ab  ");
    }

    // ── Render smoke test ──

    #[test]
    fn render_smoke_test_contains_key_strings() {
        let app = app_with(
            vec![
                provider("ollama", "Ollama", true, &["llama3.2:3b", "nomic-embed"]),
                provider("llama-cpp", "llama.cpp", false, &[]),
            ],
            vec![
                flow("f1", FlowStatus::ResourceDelivered, Some("ollama")),
                flow("f2", FlowStatus::InProgress, Some("ollama")),
            ],
        );

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, "http://127.0.0.1:1402", true))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        // Sidebar: Solana logo (braille glyph fragment) + heading + up card.
        assert!(text.contains("⣠⣶"), "missing solana logo:\n{text}");
        assert!(
            text.contains("PROVIDERS"),
            "missing sidebar heading:\n{text}"
        );
        // Chart legend (both series, colored per series).
        assert!(text.contains("tok/s ▮"), "missing token legend:\n{text}");
        assert!(
            text.contains("usdc/s ▮"),
            "missing stablecoin legend:\n{text}"
        );
        assert!(text.contains("Ollama"), "missing provider title:\n{text}");
        assert!(text.contains("llama3.2:3b"), "missing model name:\n{text}");
        assert!(text.contains("/v1/chat"), "missing request path:\n{text}");
        assert!(text.contains("web ui"), "missing web control:\n{text}");
        // No header row, and down providers are not rendered at all.
        assert!(
            !text.contains("Pay Inference"),
            "header row should be gone:\n{text}"
        );
        assert!(
            !text.contains("not detected"),
            "down provider rendered:\n{text}"
        );
        assert!(
            !text.contains("llama.cpp"),
            "down provider card rendered:\n{text}"
        );
        // The bottom DETAIL panel is gone — the table fills the rest.
        assert!(
            !text.contains("DETAIL"),
            "detail panel should be gone:\n{text}"
        );
    }

    #[test]
    fn render_smoke_test_empty_state_shows_no_providers_detected() {
        let app = app_with(vec![provider("ollama", "Ollama", false, &[])], vec![]);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &app, "http://127.0.0.1:1402", false))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        assert!(
            text.contains("no providers detected"),
            "missing empty state:\n{text}"
        );
        assert!(!text.contains("Ollama"), "down provider rendered:\n{text}");
        assert!(
            !text.contains("web ui"),
            "web control without web_url:\n{text}"
        );
    }
}
