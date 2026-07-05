//! Live TUI for `pay serve inference`: provider sidebar, live rate chart,
//! and per-connection activity table, fed by the PDB event stream
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
use pay_pdb::types::{ConnectionSummary, FlowStatus, PaymentFlow, ProviderSummary, SseMessage};
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
    /// Connection aggregates recorded before the TUI opened
    /// (`pdb.connections()`).
    pub initial_connections: Vec<ConnectionSummary>,
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
        initial_connections,
        events,
    } = args;

    let mut app = InferenceApp::new(initial_providers, initial_flows, initial_connections);

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
    /// Flows, newest first, capped at [`FLOW_CAP`] — kept for the chart
    /// buckets and the in-flight indicator (not rendered as rows).
    flows: VecDeque<PaymentFlow>,
    /// Aggregated per-connection activity, newest activity first.
    connections: Vec<ConnectionSummary>,
    pane: Pane,
    /// Index into [`Self::up_providers`] — only up providers are rendered
    /// and selectable.
    selected_provider: usize,
    /// Pinned connection id when the user moved the selection; `None`
    /// while following the newest activity.
    selected_id: Option<String>,
    /// True while the newest-activity connection is auto-selected.
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
    fn new(
        providers: Vec<ProviderSummary>,
        initial_flows: Vec<PaymentFlow>,
        initial_connections: Vec<ConnectionSummary>,
    ) -> Self {
        let mut app = Self {
            providers,
            flows: VecDeque::new(),
            connections: initial_connections,
            pane: Pane::Requests,
            selected_provider: 0,
            selected_id: None,
            follow: true,
            filter: Filter::All,
            tick: 0,
            rates: RateHistory::new(now_unix_secs()),
            last_tokens: HashMap::new(),
        };
        app.sort_connections();
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
            SseMessage::ConnectionsSnapshot { connections } => {
                self.connections = connections;
                self.sort_connections();
            }
            SseMessage::ConnectionUpdated { connection } => {
                match self.connections.iter_mut().find(|c| c.id == connection.id) {
                    Some(existing) => *existing = connection,
                    None => self.connections.push(connection),
                }
                self.sort_connections();
            }
        }
    }

    /// Newest activity first (`updated_at` is RFC3339, so lexicographic
    /// order is chronological). Stable: ties keep their relative order.
    fn sort_connections(&mut self) {
        self.connections
            .sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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

    fn matches_filter(&self, conn: &ConnectionSummary) -> bool {
        match &self.filter {
            Filter::All => true,
            Filter::Errors => conn.failed > 0,
            Filter::Provider(slug) => conn.provider.as_deref() == Some(slug.as_str()),
        }
    }

    /// Connections passing the active filter, newest activity first.
    fn filtered_connections(&self) -> Vec<&ConnectionSummary> {
        self.connections
            .iter()
            .filter(|conn| self.matches_filter(conn))
            .collect()
    }

    /// Index of the selected connection within
    /// [`Self::filtered_connections`].
    fn selected_index(&self) -> Option<usize> {
        let connections = self.filtered_connections();
        if connections.is_empty() {
            return None;
        }
        if self.follow {
            return Some(0);
        }
        self.selected_id
            .as_ref()
            .and_then(|id| connections.iter().position(|conn| conn.id == *id))
            .or(Some(0))
    }

    /// The selected connection (kept for selection tests and future detail
    /// views — the table highlights via [`Self::selected_index`]).
    #[cfg_attr(not(test), allow(dead_code))]
    fn selected_connection(&self) -> Option<&ConnectionSummary> {
        let connections = self.filtered_connections();
        self.selected_index()
            .and_then(|idx| connections.get(idx).copied())
    }

    /// Best-effort in-flight indicator: any in-progress flow that plausibly
    /// belongs to this connection (same client ip, or same provider as a
    /// fallback). Cosmetic only.
    fn connection_in_flight(&self, conn: &ConnectionSummary) -> bool {
        self.flows.iter().any(|flow| {
            flow.status == FlowStatus::InProgress
                && (flow.client_ip == conn.client_ip
                    || (conn.provider.is_some()
                        && flow.inference.as_ref().map(|i| i.provider.as_str())
                            == conn.provider.as_deref()))
        })
    }

    /// Move the connection selection by `delta` rows (positive = older
    /// activity). Any manual move pins the selection and stops following.
    fn move_selection(&mut self, delta: isize) {
        let pinned_id = {
            let connections = self.filtered_connections();
            if connections.is_empty() {
                return;
            }
            let current = self.selected_index().unwrap_or(0) as isize;
            let next = (current + delta).clamp(0, connections.len() as isize - 1) as usize;
            connections[next].id.clone()
        };
        self.selected_id = Some(pinned_id);
        self.follow = false;
    }

    fn toggle_follow(&mut self) {
        if self.follow {
            // Pin whatever currently has the newest activity.
            let pinned = self
                .filtered_connections()
                .first()
                .map(|conn| conn.id.clone());
            self.follow = false;
            self.selected_id = pinned;
        } else {
            self.follow = true;
            self.selected_id = None;
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
        // If the pinned connection got filtered out, fall back to following.
        let pinned_visible = self.selected_id.as_ref().is_some_and(|id| {
            self.filtered_connections()
                .iter()
                .any(|conn| conn.id == *id)
        });
        if !pinned_visible {
            self.selected_id = None;
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

    /// Clear the local connection + flow lists (display only — PDB history
    /// is untouched; chart history is kept, token baselines reset).
    fn clear_activity(&mut self) {
        self.connections.clear();
        self.flows.clear();
        self.last_tokens.clear();
        self.selected_id = None;
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
            KeyCode::Char('c') | KeyCode::Char('C') => self.clear_activity(),
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
    // connections table fills all remaining height.
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
    render_connections(frame, split[1], app);
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

// Connections-table column widths (characters). `models` takes whatever
// width remains.
/// Selection marker / in-flight spinner column ("▸ " / "⠹ ").
const COL_MARKER: usize = 2;
const COL_WHO: usize = 12;
const COL_PROV: usize = 7;
const COL_REQS: usize = 7;
const COL_TOK_IN: usize = 8;
const COL_TOK_OUT: usize = 8;
const COL_PAID: usize = 10;
const COL_LAST: usize = 9;
/// Minimum width of the flexible models column.
const COL_MODELS_MIN: usize = 6;

fn render_connections(frame: &mut Frame, area: Rect, app: &InferenceApp) {
    let connections = app.filtered_connections();
    let mut title = format!(" CONNECTIONS · {} ", app.connections.len());
    if app.filter != Filter::All {
        title = format!(
            " CONNECTIONS · {} · filter {} ",
            app.connections.len(),
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

    let fixed =
        COL_MARKER + COL_WHO + COL_PROV + COL_REQS + COL_TOK_IN + COL_TOK_OUT + COL_PAID + COL_LAST;
    let models_width = (inner.width as usize)
        .saturating_sub(fixed)
        .max(COL_MODELS_MIN);

    // Header row.
    let header = format!(
        "{marker}{who:<ww$}{prov:<pw$}{models:<mw$}{req:>rw$}{tin:>iw$}{tout:>ow$}{paid:>dw$}{last:>lw$}",
        marker = " ".repeat(COL_MARKER),
        who = "who",
        prov = "prov",
        models = "models",
        req = "req",
        // ↓ tokens in (prompt), ↑ tokens out (completion) — matches the
        // web UI's arrow convention.
        tin = "↓",
        tout = "↑",
        paid = "$ paid",
        last = "last",
        ww = COL_WHO,
        pw = COL_PROV,
        mw = models_width,
        rw = COL_REQS,
        iw = COL_TOK_IN,
        ow = COL_TOK_OUT,
        dw = COL_PAID,
        lw = COL_LAST,
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

    if connections.is_empty() {
        frame.render_widget(
            Paragraph::new(
                Line::from(Span::styled(
                    "waiting for connections…",
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

    for (row, (idx, conn)) in connections
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .enumerate()
    {
        let line = connection_row(app, conn, models_width, idx == selected);
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

/// One connections-table row: who (payer/ip), provider, models, ok/total
/// requests, prompt/completion token totals, $ paid, last activity.
fn connection_row(
    app: &InferenceApp,
    conn: &ConnectionSummary,
    models_width: usize,
    selected: bool,
) -> Line<'static> {
    // Selection marker wins the first column; otherwise a spinner marks
    // best-effort in-flight activity on this connection.
    let marker = if selected {
        Span::styled("▸ ", Style::default().fg(SOLANA_GREEN).bold())
    } else if app.connection_in_flight(conn) {
        Span::styled(
            format!("{} ", SPINNER[app.tick % SPINNER.len()]),
            Style::default().fg(Color::Yellow),
        )
    } else {
        Span::raw("  ")
    };

    let who = who_label(conn);
    let who_color = if conn.payer.is_some() {
        Color::Cyan
    } else {
        Color::Gray
    };
    let reqs_color = if conn.failed > 0 {
        Color::Red
    } else {
        Color::Gray
    };

    let mut spans = vec![
        marker,
        Span::styled(
            pad(&truncate_str(&who, COL_WHO - 1), COL_WHO),
            Style::default().fg(who_color),
        ),
        Span::styled(
            pad(
                &truncate_str(conn.provider.as_deref().unwrap_or("—"), COL_PROV - 1),
                COL_PROV,
            ),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            pad(
                &truncate_str(&models_label(&conn.models), models_width.saturating_sub(1)),
                models_width,
            ),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            format!("{:>width$}", reqs_label(conn), width = COL_REQS),
            Style::default().fg(reqs_color),
        ),
        Span::styled(
            format!("{:>width$}", conn.tokens_prompt, width = COL_TOK_IN),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            format!("{:>width$}", conn.tokens_completion, width = COL_TOK_OUT),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("{:>width$}", paid_label(conn.paid_usd), width = COL_PAID),
            Style::default().fg(SOLANA_GREEN),
        ),
        Span::styled(
            format!("{:>width$}", short_time(&conn.updated_at), width = COL_LAST),
            Style::default().fg(Color::DarkGray),
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

/// Who a connection belongs to: shortened payer pubkey for paid traffic,
/// client ip/host otherwise.
fn who_label(conn: &ConnectionSummary) -> String {
    conn.payer
        .as_deref()
        .map(short_pubkey)
        .unwrap_or_else(|| conn.client_ip.clone())
}

/// `F82JLphK…og` style pubkey shortening: first 4 + `…` + last 4 chars.
fn short_pubkey(pubkey: &str) -> String {
    let chars: Vec<char> = pubkey.chars().collect();
    if chars.len() <= 9 {
        return pubkey.to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// `$0.0120` — settled stablecoin total (USD, 4 decimals).
fn paid_label(paid_usd: f64) -> String {
    format!("${paid_usd:.4}")
}

/// `ok/total` request counts.
fn reqs_label(conn: &ConnectionSummary) -> String {
    format!("{}/{}", conn.ok, conn.requests)
}

/// First model plus a `+N` overflow marker (`llama3.2:3b +2`).
fn models_label(models: &[String]) -> String {
    match models.split_first() {
        None => "—".to_string(),
        Some((first, [])) => first.clone(),
        Some((first, rest)) => format!("{first} +{}", rest.len()),
    }
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

    /// Connection aggregate with the given identity/provider and activity
    /// timestamp (drives newest-first ordering).
    fn conn(
        id: &str,
        payer: Option<&str>,
        provider: Option<&str>,
        updated_at: &str,
    ) -> ConnectionSummary {
        ConnectionSummary {
            id: id.to_string(),
            payer: payer.map(str::to_string),
            client_ip: "127.0.0.1".to_string(),
            provider: provider.map(str::to_string),
            models: vec!["llama3.2:3b".to_string()],
            requests: 3,
            ok: 3,
            failed: 0,
            tokens_prompt: 120,
            tokens_completion: 450,
            paid_usd: 0.012,
            started_at: "2026-07-01T12:00:00.000Z".to_string(),
            updated_at: updated_at.to_string(),
        }
    }

    fn app_with(providers: Vec<ProviderSummary>, flows: Vec<PaymentFlow>) -> InferenceApp {
        InferenceApp::new(providers, flows, vec![])
    }

    fn app_with_conns(connections: Vec<ConnectionSummary>) -> InferenceApp {
        InferenceApp::new(vec![], vec![], connections)
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

    // ── Connection events ──

    #[test]
    fn connections_snapshot_replaces_list_sorted_newest_first() {
        let mut app = app_with_conns(vec![conn("old", None, None, "2026-07-01T11:00:00.000Z")]);
        app.apply_event(SseMessage::ConnectionsSnapshot {
            connections: vec![
                conn("a", None, None, "2026-07-01T12:00:01.000Z"),
                conn("b", None, None, "2026-07-01T12:00:05.000Z"),
            ],
        });
        let ids: Vec<&str> = app.connections.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn connection_updated_upserts_and_resorts_newest_first() {
        let mut app = app_with_conns(vec![
            conn("a", None, None, "2026-07-01T12:00:05.000Z"),
            conn("b", None, None, "2026-07-01T12:00:01.000Z"),
        ]);
        // Fresh activity on the older connection moves it to the front and
        // replaces its aggregates.
        let mut b = conn("b", None, Some("ollama"), "2026-07-01T12:00:10.000Z");
        b.requests = 4;
        b.ok = 4;
        app.apply_event(SseMessage::ConnectionUpdated { connection: b });
        let ids: Vec<&str> = app.connections.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
        assert_eq!(app.connections[0].requests, 4);
        assert_eq!(app.connections[0].provider.as_deref(), Some("ollama"));

        // An unseen id is inserted.
        app.apply_event(SseMessage::ConnectionUpdated {
            connection: conn("c", None, None, "2026-07-01T12:00:20.000Z"),
        });
        let ids: Vec<&str> = app.connections.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);
    }

    // ── Follow-latest ──

    #[test]
    fn follow_latest_tracks_connection_activity_until_user_moves_selection() {
        let mut app = app_with_conns(vec![
            conn("a", None, None, "2026-07-01T12:00:05.000Z"),
            conn("b", None, None, "2026-07-01T12:00:01.000Z"),
        ]);
        assert!(app.follow);
        assert_eq!(app.selected_connection().unwrap().id, "a");

        // While following, fresh activity moves the selection.
        app.apply_event(SseMessage::ConnectionUpdated {
            connection: conn("b", None, None, "2026-07-01T12:00:10.000Z"),
        });
        assert_eq!(app.selected_connection().unwrap().id, "b");

        // Moving the selection pins it…
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert!(!app.follow);
        assert_eq!(app.selected_connection().unwrap().id, "a");

        // …so fresh activity no longer steals it.
        app.apply_event(SseMessage::ConnectionUpdated {
            connection: conn("c", None, None, "2026-07-01T12:00:20.000Z"),
        });
        assert_eq!(app.selected_connection().unwrap().id, "a");
    }

    #[test]
    fn enter_toggles_follow_latest() {
        let mut app = app_with_conns(vec![
            conn("newest", None, None, "2026-07-01T12:00:05.000Z"),
            conn("older", None, None, "2026-07-01T12:00:01.000Z"),
        ]);
        // Pin, then re-follow via Enter.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert!(!app.follow);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.follow);
        assert_eq!(app.selected_connection().unwrap().id, "newest");

        // Enter while following pins the current newest.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.follow);
        assert_eq!(app.selected_id.as_deref(), Some("newest"));
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
    fn errors_and_provider_filters_narrow_the_connection_list() {
        let mut app = app_with_conns(vec![
            conn("ok", None, Some("ollama"), "2026-07-01T12:00:05.000Z"),
            conn("bad", None, Some("lm-studio"), "2026-07-01T12:00:01.000Z"),
        ]);
        app.connections[1].failed = 2;
        assert_eq!(app.filtered_connections().len(), 2);

        app.filter = Filter::Errors;
        let ids: Vec<&str> = app
            .filtered_connections()
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, vec!["bad"]);

        app.filter = Filter::Provider("ollama".into());
        let ids: Vec<&str> = app
            .filtered_connections()
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, vec!["ok"]);
    }

    #[test]
    fn cycling_filter_releases_pinned_connection_that_gets_filtered_out() {
        let mut app = app_with_conns(vec![
            conn("bad", None, None, "2026-07-01T12:00:05.000Z"),
            conn("ok", None, None, "2026-07-01T12:00:01.000Z"),
        ]);
        app.connections[0].failed = 1;
        // Pin the healthy (older) connection, then switch to errors.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected_id.as_deref(), Some("ok"));
        app.cycle_filter();
        assert_eq!(app.filter, Filter::Errors);
        assert!(app.follow);
        assert_eq!(app.selected_connection().unwrap().id, "bad");
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
    fn clear_empties_connections_and_flows_and_resumes_follow() {
        let mut app = InferenceApp::new(
            vec![],
            vec![flow("f1", FlowStatus::ResourceDelivered, None)],
            vec![
                conn("a", None, None, "2026-07-01T12:00:05.000Z"),
                conn("b", None, None, "2026-07-01T12:00:01.000Z"),
            ],
        );
        app.handle_key(KeyCode::Down, KeyModifiers::NONE); // pin
        app.handle_key(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(app.connections.is_empty());
        assert!(app.flows.is_empty());
        assert!(app.last_tokens.is_empty());
        assert!(app.follow);
        assert!(app.selected_id.is_none());
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
    fn connection_row_formatting_helpers() {
        // Pubkey shortening: first 4 + … + last 4; short strings untouched.
        assert_eq!(short_pubkey("F82JLphK9vCd3mgnUtog"), "F82J…Utog");
        assert_eq!(short_pubkey("shortkey1"), "shortkey1");

        // WHO: payer wins over client ip.
        let paid = conn(
            "a",
            Some("F82JLphK9vCd3mgnUtog"),
            None,
            "2026-07-01T12:00:01.000Z",
        );
        assert_eq!(who_label(&paid), "F82J…Utog");
        let anon = conn("b", None, None, "2026-07-01T12:00:01.000Z");
        assert_eq!(who_label(&anon), "127.0.0.1");

        // $ paid: 4 decimals; req counts: ok/total.
        assert_eq!(paid_label(0.012), "$0.0120");
        assert_eq!(paid_label(0.0), "$0.0000");
        assert_eq!(reqs_label(&anon), "3/3");

        // Models: first + overflow count.
        assert_eq!(models_label(&[]), "—");
        assert_eq!(models_label(&["a".to_string()]), "a");
        assert_eq!(
            models_label(&["a".to_string(), "b".to_string(), "c".to_string()]),
            "a +2"
        );
    }

    #[test]
    fn in_flight_indicator_matches_by_client_ip_or_provider() {
        let mut app = InferenceApp::new(
            vec![],
            vec![flow("f1", FlowStatus::InProgress, Some("ollama"))],
            vec![
                conn(
                    "by-provider",
                    None,
                    Some("ollama"),
                    "2026-07-01T12:00:05.000Z",
                ),
                conn("other", None, Some("lm-studio"), "2026-07-01T12:00:01.000Z"),
            ],
        );
        // Flow client_ip is "::1" (helper default); conns use 127.0.0.1 —
        // so only the provider fallback matches.
        assert!(app.connection_in_flight(&app.connections[0].clone()));
        assert!(!app.connection_in_flight(&app.connections[1].clone()));

        // ip match works regardless of provider.
        app.flows[0].client_ip = "127.0.0.1".to_string();
        assert!(app.connection_in_flight(&app.connections[1].clone()));

        // Nothing spins once the flow completes.
        app.flows[0].status = FlowStatus::ResourceDelivered;
        assert!(!app.connection_in_flight(&app.connections[0].clone()));
        assert!(!app.connection_in_flight(&app.connections[1].clone()));
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

    fn render_to_text(app: &InferenceApp, width: u16, has_web: bool) -> String {
        let backend = ratatui::backend::TestBackend::new(width, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, app, "http://127.0.0.1:1402", has_web))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn render_smoke_test_contains_key_strings() {
        let app = InferenceApp::new(
            vec![
                provider("ollama", "Ollama", true, &["llama3.2:3b", "nomic-embed"]),
                provider("llama-cpp", "llama.cpp", false, &[]),
            ],
            // Flows feed the chart + in-flight spinner only — their paths
            // must not render as table rows anymore.
            vec![
                flow("f1", FlowStatus::ResourceDelivered, Some("ollama")),
                flow("f2", FlowStatus::InProgress, Some("ollama")),
            ],
            vec![conn(
                "conn-1",
                Some("F82JLphK9vCd3mgnUtog"),
                Some("ollama"),
                "2026-07-01T12:01:24.020Z",
            )],
        );

        let text = render_to_text(&app, 120, true);

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
        assert!(text.contains("web ui"), "missing web control:\n{text}");
        // Connections table: aggregates for the one connection row.
        assert!(
            text.contains("CONNECTIONS"),
            "missing connections table title:\n{text}"
        );
        assert!(
            text.contains("F82J…Utog"),
            "missing shortened payer:\n{text}"
        );
        assert!(text.contains("$0.0120"), "missing paid aggregate:\n{text}");
        assert!(text.contains("450"), "missing completion tokens:\n{text}");
        assert!(text.contains("3/3"), "missing ok/total requests:\n{text}");
        // Per-message rows are gone: flow paths no longer render.
        assert!(
            !text.contains("/v1/chat"),
            "per-message flow rows should be gone:\n{text}"
        );
        assert!(
            !text.contains("REQUESTS"),
            "requests table should be gone:\n{text}"
        );
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
        assert!(
            !text.contains("DETAIL"),
            "detail panel should be gone:\n{text}"
        );
    }

    #[test]
    fn render_smoke_test_empty_state_shows_no_providers_detected() {
        let app = app_with(vec![provider("ollama", "Ollama", false, &[])], vec![]);

        let text = render_to_text(&app, 100, false);

        assert!(
            text.contains("no providers detected"),
            "missing empty state:\n{text}"
        );
        assert!(
            text.contains("waiting for connections…"),
            "missing empty connections state:\n{text}"
        );
        assert!(!text.contains("Ollama"), "down provider rendered:\n{text}");
        assert!(
            !text.contains("web ui"),
            "web control without web_url:\n{text}"
        );
    }
}
