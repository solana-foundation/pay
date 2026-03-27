//! Interactive TUI for configuring a payment session.
//!
//! Shown before making requests when no `--yolo` flag is set.
//! Lets the user set a spending cap and session duration — all 402
//! challenges within that budget/time are then paid automatically.

use std::io;
#[cfg(target_os = "macos")]
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::commands::ToolKind;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use pay_core::balance::{AccountBalances, ReceivedFunds};
use qrcode::QrCode;
use qrcode::render::unicode;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

const POLL_DELAY: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Result from the polling thread: what changed + current totals.
struct TopupDetected {
    received: ReceivedFunds,
    current: AccountBalances,
}

/// What the status line should show.
enum PollStatus {
    /// Initial balance fetch failed — no polling possible.
    RpcUnavailable,
    /// Waiting for the 10s delay before polling starts.
    Waiting,
    /// Actively polling for incoming funds.
    Polling,
}

/// Slider range: $0.00 to $15.00 in $0.50 increments = 30 steps, + 1 YOLO step = 31
const MAX_STEPS: usize = 31;
const STEP_AMOUNT: u64 = 500_000; // 0.50 USDC in base units (6 decimals)

const CARD_WIDTH: u16 = 36;
const CARD_BG: Color = Color::Rgb(35, 40, 50);
const TOPUP_SIDEBAR_BG: Color = Color::Rgb(24, 24, 27);
const TOPUP_MAIN_BG: Color = Color::Rgb(9, 9, 11);
const TOPUP_CARD_BG: Color = Color::Rgb(39, 39, 42);
const TOPUP_CARD_ACTIVE_BG: Color = Color::Rgb(74, 222, 128);
const TOPUP_CARD_INACTIVE_SELECTED_BG: Color = Color::Rgb(34, 84, 61);
const TOPUP_CARD_ACTIVE_FG: Color = Color::Rgb(24, 24, 27);
const SOLANA_PURPLE: Color = Color::Rgb(153, 69, 255);
const SOLANA_BLUE: Color = Color::Rgb(80, 120, 255);
const SOLANA_GREEN: Color = Color::Rgb(20, 241, 149);

/// Expiration presets: (seconds, label)
const EXPIRY_OPTIONS: &[(u64, &str)] = &[
    (60, "1m"),
    (600, "10m"),
    (1800, "30m"),
    (3600, "1h"),
    (10800, "3h"),
    (21600, "6h"),
    (43200, "12h"),
    (86400, "24h"),
];

/// Which control is active.
#[derive(PartialEq)]
enum Focus {
    Budget,
    Expiry,
}

/// The result of the session setup TUI.
pub enum SessionSetup {
    /// User approved a session with a spending cap and expiration.
    Approved { cap: u64, expires_in: u64 },
    /// User cancelled. Don't make the request.
    Cancelled,
}

/// Show the session setup TUI. Returns the user's session config.
pub fn setup_session(tool: ToolKind) -> io::Result<SessionSetup> {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return Ok(SessionSetup::Cancelled);
    }

    terminal::enable_raw_mode()?;
    let mut stdout = io::stderr();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, tool);

    let _ = terminal::disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    result
}

const DEFAULT_ONRAMP_URL: &str = "https://www.coinbase.com/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopupOption {
    TransferFromExistingAccount,
    BuyStablecoins,
}

impl TopupOption {
    fn all() -> [Self; 2] {
        [Self::TransferFromExistingAccount, Self::BuyStablecoins]
    }

    fn title(self) -> &'static str {
        match self {
            Self::TransferFromExistingAccount => "Top-up from existing account",
            Self::BuyStablecoins => "Buy stablecoins",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::TransferFromExistingAccount => "Scan or copy this Solana address",
            Self::BuyStablecoins => "Choose an onramp provider",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopupFocus {
    Methods,
    Providers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuyProvider {
    Venmo,
    Paypal,
    Coinbase,
}

impl BuyProvider {
    fn all() -> [Self; 3] {
        [Self::Coinbase, Self::Paypal, Self::Venmo]
    }

    fn title(self) -> &'static str {
        match self {
            Self::Venmo => "Venmo",
            Self::Paypal => "PayPal",
            Self::Coinbase => "Coinbase",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::Venmo => "Buy stablecoins like PYUSD",
            Self::Paypal => "Buy stablecoins like PYUSD",
            Self::Coinbase => "Buy stablecoins like USDC",
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::Venmo => "https://venmo.com/",
            Self::Paypal => "https://www.paypal.com/",
            Self::Coinbase => "https://www.coinbase.com/",
        }
    }
}

pub fn run_topup_flow(pubkey: &str, rpc_url: &str) -> pay_core::Result<()> {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        print_topup_instructions(pubkey);
        return Ok(());
    }

    terminal::enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let result = run_topup(&mut terminal, pubkey, rpc_url);

    let _ = terminal::disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    match result {
        Ok(Some(detected)) => {
            print_received(&detected.received, &detected.current);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(pay_core::Error::from(e)),
    }
}

fn run_topup(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    pubkey: &str,
    rpc_url: &str,
) -> io::Result<Option<TopupDetected>> {
    let options = TopupOption::all();
    let providers = BuyProvider::all();
    let mut selected = 0usize;
    let mut provider_selected = 0usize;
    let mut focus = TopupFocus::Methods;
    let started_at = Instant::now();

    // Fetch initial balances (best-effort; skip polling if RPC is unreachable)
    let initial_balances = pay_core::balance::get_balances(rpc_url, pubkey).ok();

    // Channel for the polling thread to report received funds
    let (tx, rx) = mpsc::channel::<TopupDetected>();
    let stop = Arc::new(AtomicBool::new(false));
    let mut polling_spawned = false;

    let cleanup = |stop: &Arc<AtomicBool>| {
        stop.store(true, Ordering::Relaxed);
    };

    loop {
        let elapsed = started_at.elapsed();
        let has_baseline = initial_balances.is_some();
        let polling_active = elapsed >= POLL_DELAY && has_baseline;

        // Spawn the polling thread once after the delay
        if polling_active && !polling_spawned {
            polling_spawned = true;
            let rpc = rpc_url.to_string();
            let pk = pubkey.to_string();
            let initial = initial_balances.clone().unwrap();
            let tx = tx.clone();
            let stop_flag = stop.clone();
            std::thread::spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    std::thread::sleep(POLL_INTERVAL);
                    if stop_flag.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(current) = pay_core::balance::get_balances(&rpc, &pk) {
                        let received = current.diff_received(&initial);
                        if received.has_any() {
                            let _ = tx.send(TopupDetected { received, current });
                            return;
                        }
                    }
                }
            });
        }

        // Check if the polling thread detected incoming funds
        if let Ok(received) = rx.try_recv() {
            cleanup(&stop);
            return Ok(Some(received));
        }

        let status = if !has_baseline {
            PollStatus::RpcUnavailable
        } else if !polling_active {
            let _secs_left = POLL_DELAY.as_secs().saturating_sub(elapsed.as_secs());
            PollStatus::Waiting
        } else {
            let _spinner_idx = (elapsed.as_millis() / 80) as usize;
            PollStatus::Polling
        };

        terminal.draw(|frame| {
            let area = frame.area();
            render_topup_selector(
                frame,
                area,
                pubkey,
                &options,
                selected,
                &providers,
                provider_selected,
                focus,
                &status,
            );
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Up => match focus {
                    TopupFocus::Methods => selected = selected.saturating_sub(1),
                    TopupFocus::Providers => {
                        provider_selected = provider_selected.saturating_sub(1);
                    }
                },
                KeyCode::Down if focus == TopupFocus::Methods && selected < options.len() - 1 => {
                    selected += 1
                }
                KeyCode::Down
                    if focus == TopupFocus::Providers
                        && provider_selected < providers.len() - 1 =>
                {
                    provider_selected += 1
                }
                KeyCode::Down => {}
                KeyCode::Left => focus = TopupFocus::Methods,
                KeyCode::Right if options[selected] == TopupOption::BuyStablecoins => {
                    focus = TopupFocus::Providers;
                }
                KeyCode::Enter => {
                    if options[selected] == TopupOption::BuyStablecoins
                        && focus == TopupFocus::Providers
                    {
                        open_url(providers[provider_selected].url())?;
                    }
                    cleanup(&stop);
                    return Ok(None);
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    cleanup(&stop);
                    return Ok(None);
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    cleanup(&stop);
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_topup_selector(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    options: &[TopupOption],
    selected: usize,
    providers: &[BuyProvider],
    provider_selected: usize,
    focus: TopupFocus,
    status: &PollStatus,
) {
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );

    let full_columns =
        Layout::horizontal([Constraint::Length(38), Constraint::Min(32)]).split(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_SIDEBAR_BG)),
        full_columns[0],
    );

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let columns =
        Layout::horizontal([Constraint::Length(38), Constraint::Min(32)]).split(chunks[0]);

    let sidebar = Layout::horizontal([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(columns[0]);
    let left = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length((options.len() as u16) * 4 - 1),
        Constraint::Min(0),
    ])
    .split(sidebar[1]);
    let right = Layout::vertical([Constraint::Length(1), Constraint::Min(8)])
        .margin(2)
        .split(columns[1]);

    let logo = Paragraph::new(vec![
        solana_logo_line(
            "",
            "⣠⣶",
            SOLANA_BLUE,
            "⣶⣶",
            SOLANA_GREEN,
            "⣶⣶⠖",
            SOLANA_GREEN,
        ),
        solana_logo_line(
            "",
            "⠲⣶",
            SOLANA_PURPLE,
            "⣶⣶",
            SOLANA_BLUE,
            "⣶⣶⣄",
            SOLANA_GREEN,
        ),
        solana_logo_line(
            "",
            "⣠⣶",
            SOLANA_PURPLE,
            "⣶⣶",
            SOLANA_PURPLE,
            "⣶⣶⠖",
            SOLANA_BLUE,
        ),
    ])
    .centered();
    frame.render_widget(logo, left[1]);

    let option_chunks = Layout::vertical(
        options
            .iter()
            .enumerate()
            .flat_map(|(idx, _)| {
                let mut rows = vec![Constraint::Length(3)];
                if idx + 1 < options.len() {
                    rows.push(Constraint::Length(1));
                }
                rows
            })
            .collect::<Vec<_>>(),
    )
    .split(left[4]);

    for (idx, option) in options.iter().enumerate() {
        let chunk_idx = idx * 2;
        let is_selected = idx == selected;
        let is_active = is_selected && focus == TopupFocus::Methods;
        let card_bg = if is_active {
            TOPUP_CARD_ACTIVE_BG
        } else if is_selected {
            TOPUP_CARD_INACTIVE_SELECTED_BG
        } else {
            TOPUP_CARD_BG
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(card_bg))
            .style(Style::default().bg(card_bg));
        let card = Paragraph::new(vec![
            Line::from(Span::styled(
                option.title(),
                Style::default()
                    .fg(if is_selected {
                        TOPUP_CARD_ACTIVE_FG
                    } else {
                        Color::White
                    })
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled(
                    if is_active { "● " } else { "  " },
                    Style::default().fg(if is_selected {
                        TOPUP_CARD_ACTIVE_FG
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    option.subtitle(),
                    Style::default().fg(if is_selected {
                        TOPUP_CARD_ACTIVE_FG
                    } else {
                        Color::Gray
                    }),
                ),
            ]),
        ])
        .block(block);
        frame.render_widget(card, option_chunks[chunk_idx]);
    }

    let active = options[selected];
    match active {
        TopupOption::TransferFromExistingAccount => render_qr_detail(frame, right[1], pubkey),
        TopupOption::BuyStablecoins => {
            render_provider_list(frame, right[1], providers, provider_selected, focus)
        }
    }

    render_topup_controls(frame, chunks[1], false, status);
}

fn render_qr_detail(frame: &mut ratatui::Frame, area: Rect, pubkey: &str) {
    let qr_lines = render_qr(pubkey).unwrap_or_else(|_| {
        vec![Line::from(Span::styled(
            "QR unavailable",
            Style::default().fg(Color::DarkGray),
        ))]
    });
    let content = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(qr_lines.len() as u16),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);

    let qr = Paragraph::new(qr_lines).centered();
    frame.render_widget(qr, content[1]);

    let pubkey_line = Paragraph::new(Line::from(Span::styled(
        pubkey,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::DIM),
    )))
    .centered();
    frame.render_widget(pubkey_line, content[3]);
}

fn render_provider_list(
    frame: &mut ratatui::Frame,
    area: Rect,
    providers: &[BuyProvider],
    selected: usize,
    focus: TopupFocus,
) {
    let lines = providers
        .iter()
        .enumerate()
        .flat_map(|(idx, provider)| {
            let is_selected = idx == selected;
            let is_active = is_selected && focus == TopupFocus::Providers;
            let title = Line::from(vec![
                Span::styled(
                    if is_selected { "⏵ " } else { "  " },
                    Style::default().fg(if is_active {
                        Color::Green
                    } else if is_selected {
                        TOPUP_CARD_INACTIVE_SELECTED_BG
                    } else {
                        Color::Black
                    }),
                ),
                Span::styled(
                    provider.title(),
                    Style::default()
                        .fg(if is_selected {
                            Color::Green
                        } else {
                            Color::White
                        })
                        .add_modifier(if is_selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
            ]);
            let subtitle = Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    provider.subtitle(),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]);

            [title, subtitle, Line::default()]
        })
        .collect::<Vec<_>>();

    frame.render_widget(Paragraph::new(lines), area);
}

fn render_qr(data: &str) -> Result<Vec<Line<'static>>, qrcode::types::QrError> {
    let code = QrCode::new(data.as_bytes())?;
    let rendered = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Dark)
        .light_color(unicode::Dense1x2::Light)
        .quiet_zone(false)
        .build();

    Ok(rendered
        .lines()
        .map(|line| {
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::White),
            ))
        })
        .collect())
}

fn render_topup_controls(
    frame: &mut ratatui::Frame,
    area: Rect,
    in_detail: bool,
    status: &PollStatus,
) {
    let mut spans = if in_detail {
        vec![
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
            Span::styled(" done  │  ", Style::default().dim()),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::styled(" back", Style::default().dim()),
        ]
    } else {
        vec![
            Span::styled("↑ ↓", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" move  │  ", Style::default().dim()),
            Span::styled("← →", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" switch pane  │  ", Style::default().dim()),
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
            Span::styled(" confirm/open  │  ", Style::default().dim()),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::styled(" skip", Style::default().dim()),
        ]
    };

    let status_spans = match status {
        PollStatus::RpcUnavailable => vec![Span::styled(
            "offline",
            Style::default().fg(Color::Red).bold(),
        )],
        PollStatus::Waiting | PollStatus::Polling => vec![Span::styled(
            "online",
            Style::default().fg(Color::Green).bold(),
        )],
    };

    let controls_width: usize = spans.iter().map(|span| span.content.len()).sum();
    let status_width: usize = status_spans.iter().map(|span| span.content.len()).sum();
    let gap = area.width as usize - controls_width.saturating_add(status_width);
    spans.push(Span::raw(" ".repeat(gap.max(1))));
    spans.extend(status_spans);

    let line = Line::from(spans);

    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(TOPUP_SIDEBAR_BG)),
        area,
    );
}

fn print_topup_instructions(pubkey: &str) {
    eprintln!("Top up your pay account:");
    eprintln!("  Address: {pubkey}");
    eprintln!("  1. Transfer funds from an existing Solana account.");
    eprintln!("  2. Buy funds using an onramp such as Coinbase: {DEFAULT_ONRAMP_URL}");
}

fn print_received(received: &ReceivedFunds, current: &AccountBalances) {
    use owo_colors::OwoColorize;

    // Line 1: what was received
    let mut parts = Vec::new();
    if received.sol_lamports > 0 {
        let sol = received.sol_lamports as f64 / 1_000_000_000.0;
        parts.push(format!("{sol:.4} SOL"));
    }
    for token in &received.tokens {
        let label = token.symbol.unwrap_or(&token.mint[..8]);
        parts.push(format!("{:.2} {label}", token.ui_amount));
    }
    if !parts.is_empty() {
        eprint!("Received ");
        eprintln!("{}", parts.join(", ").green());
    }

    // Line 2: full current balance
    let mut bal_parts = Vec::new();
    if current.sol_lamports > 0 {
        let sol = current.sol_lamports as f64 / 1_000_000_000.0;
        bal_parts.push(format!("{sol:.4} SOL"));
    }
    for token in &current.tokens {
        let label = token.symbol.unwrap_or(&token.mint[..8]);
        bal_parts.push(format!("{:.2} {label}", token.ui_amount));
    }
    if !bal_parts.is_empty() {
        eprint!("{}", "Balance: ".dimmed());
        eprintln!("{}", bal_parts.join(", ").green());
    }
}

fn open_url(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = ProcessCommand::new("open").arg(url).status()?;
        if !status.success() {
            return Err(io::Error::other("failed to open URL"));
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = url;
        Ok(())
    }
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    tool: ToolKind,
) -> io::Result<SessionSetup> {
    let mut budget_pos: usize = 2; // $1.00
    let mut expiry_pos: usize = 3; // 1h
    let mut focus = Focus::Budget;

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            render_session_setup(frame, area, budget_pos, expiry_pos, &focus, tool);
        })?;

        if event::poll(std::time::Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up | KeyCode::Tab => focus = Focus::Budget,
                KeyCode::Down | KeyCode::BackTab => focus = Focus::Expiry,
                KeyCode::Left => match focus {
                    Focus::Budget => {
                        budget_pos = budget_pos.saturating_sub(1);
                    }
                    Focus::Expiry => {
                        expiry_pos = expiry_pos.saturating_sub(1);
                    }
                },
                KeyCode::Right => match focus {
                    Focus::Budget => {
                        if budget_pos < MAX_STEPS {
                            budget_pos += 1;
                        }
                    }
                    Focus::Expiry => {
                        if expiry_pos < EXPIRY_OPTIONS.len() - 1 {
                            expiry_pos += 1;
                        }
                    }
                },
                KeyCode::Home => match focus {
                    Focus::Budget => budget_pos = 0,
                    Focus::Expiry => expiry_pos = 0,
                },
                KeyCode::End => match focus {
                    Focus::Budget => budget_pos = MAX_STEPS,
                    Focus::Expiry => expiry_pos = EXPIRY_OPTIONS.len() - 1,
                },
                KeyCode::Enter => {
                    let cap = if budget_pos >= MAX_STEPS {
                        u64::MAX // YOLO
                    } else {
                        (budget_pos as u64) * STEP_AMOUNT
                    };
                    let (expires_in, _) = EXPIRY_OPTIONS[expiry_pos];
                    return Ok(SessionSetup::Approved { cap, expires_in });
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    return Ok(SessionSetup::Cancelled);
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(SessionSetup::Cancelled);
                }
                _ => {}
            }
        }
    }
}

// ── Left panel: controls ──

fn render_session_setup(
    frame: &mut ratatui::Frame,
    area: Rect,
    budget_pos: usize,
    expiry_pos: usize,
    focus: &Focus,
    tool: ToolKind,
) {
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );

    let full_columns = Layout::horizontal([Constraint::Min(0), Constraint::Length(44)]).split(area);
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_SIDEBAR_BG)),
        full_columns[0],
    );

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let columns = Layout::horizontal([Constraint::Min(0), Constraint::Length(44)]).split(chunks[0]);

    render_left_panel(frame, columns[0], budget_pos, expiry_pos, focus);
    render_card_panel(frame, columns[1], budget_pos, expiry_pos, tool);
    render_controls(frame, chunks[1]);
}

fn render_left_panel(
    frame: &mut ratatui::Frame,
    area: Rect,
    budget_pos: usize,
    expiry_pos: usize,
    focus: &Focus,
) {
    let sidebar = Layout::horizontal([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .split(area);
    let content = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(5),
        Constraint::Min(0),
    ])
    .split(sidebar[1]);

    let logo = Paragraph::new(vec![
        solana_logo_line(
            "",
            "⣠⣶",
            SOLANA_BLUE,
            "⣶⣶",
            SOLANA_GREEN,
            "⣶⣶⠖",
            SOLANA_GREEN,
        ),
        solana_logo_line(
            "",
            "⠲⣶",
            SOLANA_PURPLE,
            "⣶⣶",
            SOLANA_BLUE,
            "⣶⣶⣄",
            SOLANA_GREEN,
        ),
        solana_logo_line(
            "",
            "⣠⣶",
            SOLANA_PURPLE,
            "⣶⣶",
            SOLANA_PURPLE,
            "⣶⣶⠖",
            SOLANA_BLUE,
        ),
    ])
    .centered();
    frame.render_widget(logo, content[1]);

    let max_w = sidebar[1].width.min(40);
    let center = |r: Rect| -> Rect {
        let h = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(max_w),
            Constraint::Min(0),
        ])
        .split(r);
        h[1]
    };

    render_budget_box(frame, center(content[3]), budget_pos, max_w, focus);
    render_expiry_box(frame, center(content[5]), expiry_pos, focus);
}

fn render_budget_box(
    frame: &mut ratatui::Frame,
    area: Rect,
    position: usize,
    box_width: u16,
    focus: &Focus,
) {
    let border_color = if *focus == Focus::Budget {
        Color::Green
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .title(" Budget ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let bar_width = (box_width as usize).saturating_sub(4);
    let num_bars = bar_width;
    let cursor_pos = (position * num_bars).checked_div(MAX_STEPS).unwrap_or(0);

    let mut bar_spans = vec![Span::raw(" ")];
    for i in 0..num_bars {
        let color = if i == cursor_pos {
            bar_color(i, num_bars, true)
        } else if i < cursor_pos {
            bar_color(i, num_bars, false)
        } else {
            Color::Rgb(50, 55, 60)
        };
        bar_spans.push(Span::styled("▐", Style::default().fg(color)));
    }

    let lines = vec![
        Line::default(),
        Line::from(bar_spans),
        Line::from(render_scale_spans(box_width)),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_expiry_box(frame: &mut ratatui::Frame, area: Rect, position: usize, focus: &Focus) {
    let border_color = if *focus == Focus::Expiry {
        Color::Green
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .title(" Expires in ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));

    let mut spans = Vec::new();
    for (i, (_, label)) in EXPIRY_OPTIONS.iter().enumerate() {
        let style = if i == position {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!(" {label} "), style));
        if i < EXPIRY_OPTIONS.len() - 1 {
            spans.push(Span::styled(
                "│",
                Style::default().fg(Color::Rgb(50, 55, 60)),
            ));
        }
    }

    let lines = vec![Line::default(), Line::from(spans), Line::default()];

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── Right panel: card column ──

const CARD_BORDER: Color = Color::Rgb(60, 65, 75);
const CARD_FACE: Color = Color::Rgb(35, 40, 50);

fn render_card_panel(
    frame: &mut ratatui::Frame,
    area: Rect,
    budget_pos: usize,
    expiry_pos: usize,
    tool: ToolKind,
) {
    // Fill entire column with background
    let bg = Block::default().style(Style::default().bg(CARD_BG));
    frame.render_widget(bg, area);

    let is_yolo = budget_pos >= MAX_STEPS;
    let dollars = (budget_pos as f64) * 0.50;
    let budget_str = if is_yolo {
        " YOLO ".to_string()
    } else {
        format!(" ${:.2} ", dollars)
    };
    let amount_bg = bar_color(budget_pos, MAX_STEPS, true);
    let (expiry_secs, _) = EXPIRY_OPTIONS[expiry_pos];
    let expires_at = std::time::SystemTime::now() + std::time::Duration::from_secs(expiry_secs);
    let datetime: chrono::DateTime<chrono::Local> = expires_at.into();
    let expiry_str = datetime.format("Exp %d/%m at %H:%M").to_string();

    let v = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(11),
        Constraint::Min(0),
    ])
    .split(area);
    let h = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(CARD_WIDTH),
        Constraint::Min(0),
    ])
    .split(v[1]);
    let card_area = h[1];

    // Clear behind card for rounded corners
    frame.render_widget(Clear, card_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(CARD_BORDER))
        .style(Style::default().bg(CARD_FACE));

    // Bottom row: budget (inverted) left, expiry right
    let inner_w = CARD_WIDTH as usize - 2; // inside borders
    let left_part = format!("  {budget_str}");
    let right_part = format!("{expiry_str}  ");
    let gap = inner_w.saturating_sub(left_part.len() + right_part.len());

    let tool_lines: Vec<Line> = match tool {
        ToolKind::Claude => {
            let cc = Color::Rgb(218, 119, 86); // Claude Code orange #DA7756
            vec![
                Line::default(),
                Line::from(Span::styled("   ▐▛███▜▌", Style::default().fg(cc))),
                Line::from(Span::styled("  ▝▜█████▛▘  claude", Style::default().fg(cc))),
                Line::from(Span::styled("    ▘▘ ▝▝", Style::default().fg(cc))),
                Line::default(),
            ]
        }
        ToolKind::Codex => vec![
            Line::default(),
            solana_logo_line(
                "  ",
                "⣠⣶",
                SOLANA_BLUE,
                "⣶⣶",
                SOLANA_GREEN,
                "⣶⣶⠖",
                SOLANA_GREEN,
            ),
            solana_logo_line(
                "  ",
                "⠲⣶",
                SOLANA_PURPLE,
                "⣶⣶",
                SOLANA_BLUE,
                "⣶⣶⣄",
                SOLANA_GREEN,
            ),
            solana_logo_line(
                "  ",
                "⣠⣶",
                SOLANA_PURPLE,
                "⣶⣶",
                SOLANA_PURPLE,
                "⣶⣶⠖",
                SOLANA_BLUE,
            ),
            Line::from(Span::styled(
                "  codex",
                Style::default().fg(Color::DarkGray),
            )),
        ],
        _ => {
            let tool_label = match tool {
                ToolKind::Curl => "curl",
                ToolKind::Wget => "wget",
                ToolKind::Httpie => "httpie",
                ToolKind::Fetch => "fetch",
                ToolKind::Mcp => "mcp",
                ToolKind::Claude | ToolKind::Codex => unreachable!(),
            };
            vec![
                Line::default(),
                Line::from(Span::styled(
                    format!("  {tool_label}"),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::default(),
                Line::default(),
            ]
        }
    };

    let mut lines = tool_lines;
    lines.extend([
        Line::from(Span::styled(
            "  4402  ****  ****  0402",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                budget_str,
                Style::default()
                    .fg(CARD_FACE)
                    .bg(amount_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(gap), Style::default()),
            Span::styled(right_part, Style::default().fg(Color::DarkGray)),
        ]),
    ]);

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, card_area);
}

// ── Helpers ──

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

/// Interpolate bar color from green → yellow → red based on position.
fn bar_color(index: usize, total: usize, bright: bool) -> Color {
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

fn render_scale_spans(box_width: u16) -> Vec<Span<'static>> {
    let bar_width = (box_width as usize).saturating_sub(4);
    let labels = ["$0", "$5", "$10", "$15", "YOLO"];

    let mut spans = vec![Span::raw(" ")];
    for (i, label) in labels.iter().enumerate() {
        let pos = if i == labels.len() - 1 {
            bar_width
        } else {
            (i * bar_width) / (labels.len() - 1)
        };

        let current_len: usize = spans.iter().map(|s| s.content.len()).sum();
        let target = pos + 1;
        if target > current_len {
            spans.push(Span::raw(" ".repeat(target - current_len)));
        }
        spans.push(Span::styled(
            label.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    spans
}

fn render_controls(frame: &mut ratatui::Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled("← →", Style::default().fg(Color::Cyan).bold()),
        Span::styled(" adjust  ", Style::default().dim()),
        Span::styled("↑ ↓", Style::default().fg(Color::Cyan).bold()),
        Span::styled(" switch  │  ", Style::default().dim()),
        Span::styled("Enter", Style::default().fg(Color::Green).bold()),
        Span::styled(" start  │  ", Style::default().dim()),
        Span::styled("Esc", Style::default().fg(Color::Red).bold()),
        Span::styled(" cancel", Style::default().dim()),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(TOPUP_SIDEBAR_BG)),
        area,
    );
}
