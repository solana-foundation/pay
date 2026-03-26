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

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const POLL_DELAY: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Result from the polling thread: what changed + current totals.
struct TopupDetected {
    received: ReceivedFunds,
    current: AccountBalances,
}

/// What the status line should show.
enum PollStatus<'a> {
    /// Initial balance fetch failed — no polling possible.
    RpcUnavailable(&'a str),
    /// Waiting for the 10s delay before polling starts.
    Waiting { secs_left: u64 },
    /// Actively polling for incoming funds.
    Polling { spinner_idx: usize },
}

/// Slider range: $0.00 to $15.00 in $0.50 increments = 30 steps, + 1 YOLO step = 31
const MAX_STEPS: usize = 31;
const STEP_AMOUNT: u64 = 500_000; // 0.50 USDC in base units (6 decimals)

const CARD_WIDTH: u16 = 36;
const CARD_BG: Color = Color::Rgb(35, 40, 50);

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
    MobileWallet,
    CryptoBro,
    Onramp,
}

impl TopupOption {
    fn all() -> [Self; 3] {
        [Self::MobileWallet, Self::CryptoBro, Self::Onramp]
    }

    fn title(self) -> &'static str {
        match self {
            Self::MobileWallet => "I have a mobile wallet and can scan a QR code",
            Self::CryptoBro => "I have a crypto bro and ask onboard",
            Self::Onramp => "I'll crypto onramp (open a webpage)",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::MobileWallet => "Mobile wallet",
            Self::CryptoBro => "Crypto bro",
            Self::Onramp => "Onramp",
        }
    }

    fn detail_title(self) -> &'static str {
        match self {
            Self::MobileWallet => "Scan this QR code with your wallet",
            Self::CryptoBro => "Send this address to your crypto bro",
            Self::Onramp => "Opening your onramp",
        }
    }

    fn body(self, pubkey: &str) -> Vec<String> {
        match self {
            Self::MobileWallet => vec![
                "Fund this wallet with SOL so you can start paying for APIs.".to_string(),
                "Any Solana wallet that can scan wallet addresses should work.".to_string(),
                format!("Address: {pubkey}"),
            ],
            Self::CryptoBro => vec![
                "Send this wallet address to the person onboarding you.".to_string(),
                "Ask them to transfer enough SOL to cover your first paid requests.".to_string(),
                format!("Address: {pubkey}"),
            ],
            Self::Onramp => vec![
                "A browser window will open so you can buy SOL.".to_string(),
                "After purchase, withdraw or send the funds to this address:".to_string(),
                pubkey.to_string(),
            ],
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
    let mut selected = 0usize;
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
            PollStatus::RpcUnavailable(rpc_url)
        } else if !polling_active {
            let secs_left = POLL_DELAY.as_secs().saturating_sub(elapsed.as_secs());
            PollStatus::Waiting { secs_left }
        } else {
            PollStatus::Polling {
                spinner_idx: (elapsed.as_millis() / 80) as usize,
            }
        };

        terminal.draw(|frame| {
            let area = frame.area();
            render_topup_selector(frame, area, pubkey, &options, selected, &status);
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down if selected < options.len() - 1 => selected += 1,
                KeyCode::Down => {}
                KeyCode::Enter => {
                    if options[selected] == TopupOption::Onramp {
                        open_onramp(pubkey)?;
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

fn render_topup_selector(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    options: &[TopupOption],
    selected: usize,
    status: &PollStatus,
) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1), // status line (polling indicator)
        Constraint::Length(2),
    ])
    .margin(1)
    .split(area);
    let columns =
        Layout::horizontal([Constraint::Length(38), Constraint::Min(32)]).split(chunks[1]);
    let left = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length((options.len() as u16) * 4),
        Constraint::Min(0),
    ])
    .split(columns[0]);
    let right = Layout::vertical([Constraint::Length(3), Constraint::Min(8)]).split(columns[1]);

    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            "Top up your pay account",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Pick how you want to get your first SOL onto this wallet.",
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(header, chunks[0]);

    let address = Paragraph::new(vec![
        Line::from(Span::styled(
            "Wallet address",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            pubkey,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
    ]);
    frame.render_widget(address, left[0]);

    let option_chunks = Layout::vertical(
        options
            .iter()
            .map(|_| Constraint::Length(4))
            .collect::<Vec<_>>(),
    )
    .split(left[1]);

    for (idx, option) in options.iter().enumerate() {
        let is_selected = idx == selected;
        let lines = vec![
            Line::from(Span::styled(
                if is_selected {
                    format!("> {}", option.short_label())
                } else {
                    format!("  {}", option.short_label())
                },
                Style::default()
                    .fg(if is_selected {
                        Color::Cyan
                    } else {
                        Color::White
                    })
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                option.title(),
                Style::default().fg(Color::DarkGray),
            )),
        ];

        let card = Paragraph::new(lines);
        frame.render_widget(card, option_chunks[idx]);
    }

    let active = options[selected];
    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            active.detail_title(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            match active {
                TopupOption::Onramp => "Press Enter to open the onramp page.",
                _ => "Press Enter when you're done funding this wallet.",
            },
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(header, right[0]);

    match active {
        TopupOption::MobileWallet => render_qr_detail(frame, right[1], pubkey),
        TopupOption::CryptoBro | TopupOption::Onramp => {
            render_address_detail(frame, right[1], pubkey, active)
        }
    }

    // Status line
    let status_widget = match status {
        PollStatus::RpcUnavailable(url) => Some(Paragraph::new(Line::from(vec![
            Span::styled("! ", Style::default().fg(Color::Red)),
            Span::styled(
                format!("Cannot reach RPC ({url}) — polling disabled"),
                Style::default().fg(Color::DarkGray),
            ),
        ]))),
        PollStatus::Waiting { secs_left } => Some(Paragraph::new(Line::from(Span::styled(
            format!("  Polling starts in {secs_left}s…"),
            Style::default().fg(Color::Rgb(60, 65, 75)),
        )))),
        PollStatus::Polling { spinner_idx } => {
            let ch = SPINNER[spinner_idx % SPINNER.len()];
            Some(Paragraph::new(Line::from(vec![
                Span::styled(format!("{ch} "), Style::default().fg(Color::Yellow)),
                Span::styled(
                    "Watching for incoming funds…",
                    Style::default().fg(Color::DarkGray),
                ),
            ])))
        }
    };
    if let Some(w) = status_widget {
        frame.render_widget(w, chunks[2]);
    }

    render_topup_controls(frame, chunks[3], false);
}

fn render_qr_detail(frame: &mut ratatui::Frame, area: Rect, pubkey: &str) {
    let mut lines = render_qr(pubkey).unwrap_or_else(|_| {
        vec![Line::from(Span::styled(
            "QR unavailable",
            Style::default().fg(Color::DarkGray),
        ))]
    });
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        pubkey,
        Style::default().fg(Color::Cyan),
    )));

    let body = Paragraph::new(lines).centered();
    frame.render_widget(body, area);
}

fn render_address_detail(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    option: TopupOption,
) {
    let lines = option
        .body(pubkey)
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::White))))
        .collect::<Vec<_>>();

    let body = Paragraph::new(lines);
    frame.render_widget(body, area);
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

fn render_topup_controls(frame: &mut ratatui::Frame, area: Rect, in_detail: bool) {
    let line = if in_detail {
        Line::from(vec![
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
            Span::styled(" done  │  ", Style::default().dim()),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::styled(" back", Style::default().dim()),
        ])
    } else {
        Line::from(vec![
            Span::styled("↑ ↓", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" choose  │  ", Style::default().dim()),
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
            Span::styled(" continue  │  ", Style::default().dim()),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::styled(" skip", Style::default().dim()),
        ])
    };

    frame.render_widget(Paragraph::new(vec![Line::default(), line]), area);
}

fn print_topup_instructions(pubkey: &str) {
    eprintln!("Top up your pay account:");
    eprintln!("  Address: {pubkey}");
    eprintln!("  1. Scan the address with a mobile wallet.");
    eprintln!("  2. Send the address to the person onboarding you.");
    eprintln!("  3. Buy SOL via an onramp: {DEFAULT_ONRAMP_URL}");
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

fn open_onramp(_pubkey: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let status = ProcessCommand::new("open")
            .arg(DEFAULT_ONRAMP_URL)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("failed to open onramp URL"));
        }
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    {
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

            let right_col_width = CARD_WIDTH + 4; // card + 2 padding each side
            let columns =
                Layout::horizontal([Constraint::Min(30), Constraint::Length(right_col_width)])
                    .split(area);

            render_left_panel(frame, columns[0], budget_pos, expiry_pos, &focus);
            render_card_panel(frame, columns[1], budget_pos, expiry_pos, tool);
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

fn render_left_panel(
    frame: &mut ratatui::Frame,
    area: Rect,
    budget_pos: usize,
    expiry_pos: usize,
    focus: &Focus,
) {
    let chunks = Layout::vertical([
        Constraint::Min(0),    // top spacer
        Constraint::Length(5), // budget box
        Constraint::Length(1), // spacer
        Constraint::Length(5), // expiry box
        Constraint::Min(0),    // bottom spacer
        Constraint::Length(2), // controls (pinned to bottom)
    ])
    .split(area);

    // Center horizontally with max width
    let max_w = 50.min(area.width.saturating_sub(4));
    let center = |r: Rect| -> Rect {
        let h = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(max_w),
            Constraint::Min(0),
        ])
        .split(r);
        h[1]
    };

    render_budget_box(frame, center(chunks[1]), budget_pos, max_w, focus);
    render_expiry_box(frame, center(chunks[3]), expiry_pos, focus);
    render_controls(frame, center(chunks[5]));
}

fn render_budget_box(
    frame: &mut ratatui::Frame,
    area: Rect,
    position: usize,
    box_width: u16,
    focus: &Focus,
) {
    let border_color = if *focus == Focus::Budget {
        Color::Cyan
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
        Color::Cyan
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
                .bg(Color::Cyan)
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
    let (expiry_secs, _) = EXPIRY_OPTIONS[expiry_pos];
    let expires_at = std::time::SystemTime::now() + std::time::Duration::from_secs(expiry_secs);
    let datetime: chrono::DateTime<chrono::Local> = expires_at.into();
    let expiry_str = datetime.format("Exp %d/%m at %H:%M").to_string();

    // Layout: top padding + card + rest is background
    let v = Layout::vertical([
        Constraint::Length(2),  // top padding
        Constraint::Length(11), // card
        Constraint::Min(0),     // rest (background)
    ])
    .split(area);

    // Center card horizontally with 2-char padding on each side
    let h = Layout::horizontal([
        Constraint::Length(2),
        Constraint::Length(CARD_WIDTH),
        Constraint::Length(2),
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
        _ => {
            let tool_label = match tool {
                ToolKind::Curl => "curl",
                ToolKind::Wget => "wget",
                ToolKind::Httpie => "httpie",
                ToolKind::Fetch => "fetch",
                ToolKind::Codex => "codex",
                ToolKind::Mcp => "mcp",
                ToolKind::Claude => unreachable!(),
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
                    .bg(Color::White)
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

/// Interpolate bar color from green → yellow → red based on position.
fn bar_color(index: usize, total: usize, bright: bool) -> Color {
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
    frame.render_widget(Paragraph::new(vec![Line::default(), line]), area);
}
