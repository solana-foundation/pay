//! Interactive TUI for configuring a payment session.
//!
//! Shown before making requests when no `--yolo` flag is set.
//! Lets the user set a spending cap and session duration — all 402
//! challenges within that budget/time are then paid automatically.

use std::io;
use std::io::Write;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::commands::ToolKind;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use pay_core::client::balance::{AccountBalances, ReceivedFunds, ReceivedToken};
use qrcode::{Color as QrColor, QrCode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

const POLL_DELAY: Duration = Duration::from_secs(4);
const POLL_COUNTDOWN: Duration = Duration::from_secs(4);

/// Result from the polling thread: what changed + current totals.
struct TopupDetected {
    received: ReceivedFunds,
    /// On-chain transaction hash for the funding transfer (only present
    /// when MoonPay reports completion via the external-id status endpoint).
    tx_hash: Option<String>,
}

/// Status updates from the MoonPay poller thread.
#[derive(Debug, PartialEq, Eq)]
enum OnrampUpdate {
    /// MoonPay has reported a completed payment + on-chain transfer.
    Completed {
        tx_hash: Option<String>,
        crypto_amount: Option<String>,
        crypto_currency: Option<String>,
    },
    /// MoonPay has reported failure. The TUI surfaces the reason and stays
    /// open so the user can retry.
    Failed { reason: String },
}

struct RenderedQr {
    lines: Vec<Line<'static>>,
    width: u16,
    height: u16,
}

/// What the status line should show.
enum PollStatus {
    /// Initial balance fetch failed — no polling possible.
    RpcUnavailable,
    /// Waiting for the initial delay before first check.
    Waiting { secs_left: u64 },
    /// Currently fetching balances from RPC.
    Checking { spinner_idx: usize },
    /// Countdown until the next automatic refresh.
    Countdown { secs_left: u64 },
}

/// Slider range: $0.00 to $15.00 in $0.50 increments = 30 steps, + 1 YOLO step = 31
const MAX_STEPS: usize = 31;
const STEP_AMOUNT: u64 = 500_000; // 0.50 USDC in base units (6 decimals)

/// Topup amount slider: 0 = any amount, 1-25 = $1 to $25 in $1 steps
const TOPUP_MAX_STEPS: usize = 25;
const TOPUP_STEP_USDC: f64 = 1.0;
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const MOONPAY_BUY_URL: &str = "https://buy.moonpay.com/v2/buy";
const MOONPAY_EXTERNAL_TX_ENDPOINT: &str = "https://api.moonpay.com/v1/transactions/ext";
/// Polling cadence for MoonPay's external transaction status endpoint.
const ONRAMP_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Hard cap on the on-ramp poller — if we don't see a terminal status by then,
/// the thread exits silently. (The user can press `r` to relaunch.)
const ONRAMP_POLL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

const CARD_WIDTH: u16 = 36;
const CARD_BG: Color = Color::Rgb(35, 40, 50);
const TOPUP_SIDEBAR_BG: Color = Color::Rgb(24, 24, 27);
const TOPUP_MAIN_BG: Color = Color::Rgb(9, 9, 11);
const TOPUP_CARD_BG: Color = Color::Rgb(39, 39, 42);
const TOPUP_CARD_ACTIVE_BG: Color = Color::Rgb(74, 222, 128);
const TOPUP_CARD_INACTIVE_SELECTED_BG: Color = Color::Rgb(34, 84, 61);
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

/// Run a closure with a full-screen terminal, restoring state on exit.
fn with_terminal<T>(
    f: impl FnOnce(&mut Terminal<CrosstermBackend<io::Stderr>>) -> io::Result<T>,
) -> io::Result<T> {
    terminal::enable_raw_mode()?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stderr);
    let mut terminal = Terminal::new(backend)?;

    let result = f(&mut terminal);

    let _ = terminal::disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    result
}

/// Show the session setup TUI. Returns the user's session config.
pub fn setup_session(tool: ToolKind, account_name: &str) -> io::Result<SessionSetup> {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return Ok(SessionSetup::Cancelled);
    }

    with_terminal(|terminal| run(terminal, tool, account_name))
}

const DEFAULT_ONRAMP_URL: &str = "https://buy.moonpay.com/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopupOption {
    TransferFromExistingAccount,
    BuyStablecoins,
}

impl TopupOption {
    fn all() -> [Self; 2] {
        // Buy stablecoins is the recommended flow for new users — list it first
        // so it's the default-highlighted card.
        [Self::BuyStablecoins, Self::TransferFromExistingAccount]
    }

    fn title(self) -> &'static str {
        match self {
            Self::TransferFromExistingAccount => "Top-up from Mobile wallet",
            Self::BuyStablecoins => "Buy stablecoins",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::TransferFromExistingAccount => "Scan with any Solana wallet",
            Self::BuyStablecoins => "Pay with card, Apple Pay, or bank",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TopupFocus {
    Methods,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OnrampPaymentMethod {
    Paypal,
    Venmo,
}

impl OnrampPaymentMethod {
    fn default() -> Self {
        Self::Paypal
    }

    fn title(self) -> &'static str {
        match self {
            Self::Paypal => "PayPal",
            Self::Venmo => "Venmo",
        }
    }

    fn query_value(self) -> &'static str {
        match self {
            Self::Paypal => "paypal",
            Self::Venmo => "venmo",
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Paypal => Self::Venmo,
            Self::Venmo => Self::Paypal,
        }
    }

    fn next(self) -> Self {
        self.previous()
    }
}

/// Resolve the redirect host from `PAY_ONRAMP_HOST`, falling back to
/// `https://pay.sh`. Trailing slashes are stripped so callers can `format!`
/// without double-slash hazards.
fn resolve_onramp_host() -> String {
    let raw = std::env::var("PAY_ONRAMP_HOST").unwrap_or_else(|_| "https://pay.sh".to_string());
    raw.trim_end_matches('/').to_string()
}

/// Run the interactive top-up TUI for an account.
///
/// Presents two options to the user:
///
/// 1. **Buy stablecoins** (default) — copies the destination wallet address to
///    the clipboard, shows a brief launch animation, then opens MoonPay
///    directly in the user's browser and polls MoonPay's
///    `GET /v1/transactions/ext/{externalTransactionId}?apiKey=...` endpoint
///    until a terminal status arrives. `PAY_ONRAMP_HOST` only controls the
///    browser redirect target (`{host}/onramp/done`), defaulting to
///    `https://pay.sh`.
/// 2. **Top-up from mobile wallet** — renders a Solana Pay QR code that any
///    Solana wallet can scan, while polling the RPC for incoming SOL/SPL token
///    balance changes against `pubkey`.
///
/// Both paths run concurrently: an on-chain balance increase or a MoonPay
/// `completed` webhook will end the flow with `Ok(Some(_))`. The user can
/// dismiss the TUI at any time with `Esc`/`q`/`Ctrl-C`, which yields
/// `Ok(None)`.
///
/// When stderr is not a TTY (e.g. CI, piped output), this falls back to
/// printing static top-up instructions and returns `Ok(None)` immediately.
///
/// # Parameters
/// - `pubkey`: base58 destination address shown in the QR code and threaded
///   into MoonPay as the locked `walletAddress`.
/// - `rpc_url`: Solana JSON-RPC endpoint used by the background balance poller
///   to detect on-chain top-ups.
/// - `account_name`: human-readable account label rendered in the TUI.
///
/// # Returns
/// - `Ok(Some(TopupCompletion))` if funds landed (either path). The completion
///   carries a synthesized [`ReceivedFunds`] for amount formatting and an
///   optional on-chain tx hash (only populated for MoonPay completions).
/// - `Ok(None)` if the user dismissed without funding.
/// - `Err(_)` if the terminal could not be entered or restored.
pub fn run_topup_flow(
    pubkey: &str,
    rpc_url: &str,
    account_name: &str,
) -> pay_core::Result<Option<TopupCompletion>> {
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        print_topup_instructions(pubkey);
        return Ok(None);
    }

    let onramp_host = resolve_onramp_host();
    let result =
        with_terminal(|terminal| run_topup(terminal, pubkey, rpc_url, account_name, &onramp_host))?;

    Ok(result.map(|d| TopupCompletion {
        received: d.received,
        tx_hash: d.tx_hash,
    }))
}

/// Funds detected during a topup TUI session.
pub struct TopupCompletion {
    pub received: ReceivedFunds,
    /// On-chain tx hash, when known (only set for MoonPay completions).
    pub tx_hash: Option<String>,
}

fn run_topup(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    pubkey: &str,
    rpc_url: &str,
    account_name: &str,
    onramp_host: &str,
) -> io::Result<Option<TopupDetected>> {
    let options = TopupOption::all();
    let mut selected = 0usize;
    let mut payment_method = OnrampPaymentMethod::default();
    let focus = TopupFocus::Methods;
    let mut amount_pos: usize = 10; // default $10
    let started_at = Instant::now();

    // Active MoonPay session (set after the user hits Enter on "Buy stablecoins").
    let mut onramp: Option<OnrampSession> = None;
    let mut onramp_notice: Option<String> = None;
    let mut onramp_error: Option<String> = None;
    let (otx, orx) = mpsc::channel::<OnrampUpdate>();

    // Fetch initial balances (best-effort; skip polling if RPC is unreachable)
    let initial_balances = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(pay_core::client::balance::get_balances(rpc_url, pubkey))
        .ok();

    // Channel for background balance checks
    let (tx, rx) = mpsc::channel::<TopupDetected>();
    let mut last_check_at: Option<Instant> = None;
    let mut checking = false;

    // Trigger a balance check on the background thread.
    let trigger_check = |tx: &mpsc::Sender<TopupDetected>,
                         initial: &AccountBalances,
                         rpc_url: &str,
                         pubkey: &str| {
        let rpc = rpc_url.to_string();
        let pk = pubkey.to_string();
        let initial = initial.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            if let Ok(current) = rt.block_on(pay_core::client::balance::get_balances(&rpc, &pk)) {
                let received = current.diff_received(&initial);
                if received.has_any() {
                    let _ = tx.send(TopupDetected {
                        received,
                        tx_hash: None,
                    });
                }
            }
            // Send a sentinel None to signal "check finished, nothing found"
        });
    };

    loop {
        let elapsed = started_at.elapsed();
        let has_baseline = initial_balances.is_some();
        let past_delay = elapsed >= POLL_DELAY;

        // Auto-trigger first check after POLL_DELAY, then every POLL_COUNTDOWN
        if has_baseline && past_delay && !checking {
            let should_check = match last_check_at {
                None => true,
                Some(t) => t.elapsed() >= POLL_COUNTDOWN,
            };
            if should_check {
                checking = true;
                last_check_at = Some(Instant::now());
                trigger_check(&tx, initial_balances.as_ref().unwrap(), rpc_url, pubkey);
            }
        }

        // Check if a background check detected incoming funds
        if let Ok(received) = rx.try_recv() {
            blink_checkmark(
                terminal,
                pubkey,
                account_name,
                &options,
                selected,
                focus,
                amount_pos,
                payment_method,
                onramp.as_ref(),
                onramp_notice.as_deref(),
                onramp_error.as_deref(),
            )?;
            return Ok(Some(received));
        }

        // Drain on-ramp poller updates.
        while let Ok(update) = orx.try_recv() {
            match update {
                OnrampUpdate::Completed {
                    tx_hash,
                    crypto_amount,
                    crypto_currency,
                } => {
                    let received = synthesize_received_funds(&crypto_amount, &crypto_currency);
                    let detected = TopupDetected { received, tx_hash };
                    blink_checkmark(
                        terminal,
                        pubkey,
                        account_name,
                        &options,
                        selected,
                        focus,
                        amount_pos,
                        payment_method,
                        onramp.as_ref(),
                        onramp_notice.as_deref(),
                        onramp_error.as_deref(),
                    )?;
                    return Ok(Some(detected));
                }
                OnrampUpdate::Failed { reason } => {
                    onramp_error = Some(reason);
                }
            }
        }
        // If we were checking and the thread finished (channel empty), mark done
        if checking && last_check_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(6)) {
            checking = false; // RPC timed out or returned no change
        }

        let status = if !has_baseline {
            PollStatus::RpcUnavailable
        } else if !past_delay {
            let secs_left = POLL_DELAY.as_secs().saturating_sub(elapsed.as_secs());
            PollStatus::Waiting { secs_left }
        } else if checking {
            let spinner_idx = (elapsed.as_millis() / 80) as usize;
            PollStatus::Checking { spinner_idx }
        } else {
            let since_last = last_check_at.map(|t| t.elapsed()).unwrap_or_default();
            let secs_left = POLL_COUNTDOWN
                .as_secs()
                .saturating_sub(since_last.as_secs());
            PollStatus::Countdown { secs_left }
        };

        terminal.draw(|frame| {
            let area = frame.area();
            render_topup_selector(
                frame,
                area,
                pubkey,
                account_name,
                &options,
                selected,
                focus,
                &status,
                amount_pos,
                payment_method,
                None,
                onramp.as_ref(),
                onramp_notice.as_deref(),
                onramp_error.as_deref(),
            );
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Down if selected < options.len() - 1 => {
                    selected += 1;
                }
                KeyCode::Down => {}
                KeyCode::Left => {
                    if options[selected] == TopupOption::TransferFromExistingAccount {
                        amount_pos = amount_pos.saturating_sub(1);
                    } else if options[selected] == TopupOption::BuyStablecoins && onramp.is_none() {
                        payment_method = payment_method.previous();
                    }
                }
                KeyCode::Right => {
                    if options[selected] == TopupOption::TransferFromExistingAccount
                        && amount_pos < TOPUP_MAX_STEPS
                    {
                        amount_pos += 1;
                    } else if options[selected] == TopupOption::BuyStablecoins && onramp.is_none() {
                        payment_method = payment_method.next();
                    }
                }
                KeyCode::Enter => {
                    if options[selected] == TopupOption::BuyStablecoins {
                        if onramp.is_none() {
                            let copied_to_clipboard = copy_to_clipboard(pubkey).is_ok();
                            animate_onramp_launch(
                                terminal,
                                TopupLaunchView {
                                    pubkey,
                                    account_name,
                                    options: &options,
                                    selected,
                                    focus,
                                    status: &status,
                                    amount_pos,
                                    payment_method,
                                },
                                copied_to_clipboard,
                            )?;
                            match launch_onramp_session(onramp_host, pubkey, payment_method, &otx) {
                                Ok(session) => {
                                    onramp_notice = None;
                                    onramp_error = None;
                                    onramp = Some(session);
                                }
                                Err(reason) => {
                                    onramp_notice = None;
                                    onramp_error = Some(reason);
                                    onramp = None;
                                }
                            }
                        }
                    } else {
                        return Ok(None);
                    }
                }
                KeyCode::Char('r') | KeyCode::Char('R')
                    if options[selected] == TopupOption::BuyStablecoins && onramp.is_some() =>
                {
                    // Reopen the existing MoonPay tab without rotating the
                    // externalTransactionId — the in-flight session is still valid.
                    if let Some(session) = onramp.as_ref() {
                        let _ = open_url(&session.url);
                    }
                }
                KeyCode::Char('r') | KeyCode::Char('R')
                    if has_baseline && past_delay && !checking =>
                {
                    checking = true;
                    last_check_at = Some(Instant::now());
                    trigger_check(&tx, initial_balances.as_ref().unwrap(), rpc_url, pubkey);
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    return Ok(None);
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
}

/// Blink state passed into the normal render to replace the QR with a checkmark.
struct BlinkState {
    visible: bool,
}

/// Active MoonPay session state surfaced into the TUI.
#[derive(Debug)]
struct OnrampSession {
    external_id: String,
    url: String,
    payment_method: OnrampPaymentMethod,
    started_at: Instant,
}

#[derive(Clone, Copy)]
struct TopupLaunchView<'a> {
    pubkey: &'a str,
    account_name: &'a str,
    options: &'a [TopupOption],
    selected: usize,
    focus: TopupFocus,
    status: &'a PollStatus,
    amount_pos: usize,
    payment_method: OnrampPaymentMethod,
}

#[allow(clippy::too_many_arguments)]
fn blink_checkmark(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    pubkey: &str,
    account_name: &str,
    options: &[TopupOption],
    selected: usize,
    focus: TopupFocus,
    amount_pos: usize,
    payment_method: OnrampPaymentMethod,
    onramp: Option<&OnrampSession>,
    onramp_notice: Option<&str>,
    onramp_error: Option<&str>,
) -> io::Result<()> {
    for i in 0..5 {
        let visible = i % 2 == 0;
        let blink = Some(BlinkState { visible });
        terminal.draw(|frame| {
            let area = frame.area();
            render_topup_selector(
                frame,
                area,
                pubkey,
                account_name,
                options,
                selected,
                focus,
                &PollStatus::RpcUnavailable, // status bar doesn't matter during blink
                amount_pos,
                payment_method,
                blink.as_ref(),
                onramp,
                onramp_notice,
                onramp_error,
            );
        })?;
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

fn animate_onramp_launch(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    view: TopupLaunchView<'_>,
    copied_to_clipboard: bool,
) -> io::Result<()> {
    let frames: &[&str] = if copied_to_clipboard {
        &[
            "Copying wallet address.",
            "Copying wallet address..",
            "Wallet address copied. Paste it into MoonPay when asked which wallet to fund.",
            "Wallet address copied. Opening MoonPay...",
        ]
    } else {
        &[
            "Clipboard copy unavailable.",
            "When MoonPay asks which wallet to fund, paste the pubkey shown above.",
            "Opening MoonPay with this wallet address locked in..",
            "Opening MoonPay with this wallet address locked in...",
        ]
    };

    for notice in frames {
        terminal.draw(|frame| {
            let area = frame.area();
            render_topup_selector(
                frame,
                area,
                view.pubkey,
                view.account_name,
                view.options,
                view.selected,
                view.focus,
                view.status,
                view.amount_pos,
                view.payment_method,
                None,
                None,
                Some(notice),
                None,
            );
        })?;
        std::thread::sleep(Duration::from_millis(250));
    }

    Ok(())
}

fn render_success_checkmark(frame: &mut ratatui::Frame, area: Rect, visible: bool) {
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

#[allow(clippy::too_many_arguments)]
fn render_topup_selector(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    account_name: &str,
    options: &[TopupOption],
    selected: usize,
    focus: TopupFocus,
    status: &PollStatus,
    amount_pos: usize,
    payment_method: OnrampPaymentMethod,
    blink: Option<&BlinkState>,
    onramp: Option<&OnrampSession>,
    onramp_notice: Option<&str>,
    onramp_error: Option<&str>,
) {
    frame.render_widget(Clear, area);
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
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        full_columns[1],
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
    // Ensure right column has dark background before content.
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        columns[1],
    );
    let right = Layout::vertical([Constraint::Length(1), Constraint::Min(8)])
        .margin(2)
        .split(columns[1]);

    frame.render_widget(Paragraph::new(solana_logo("")).centered(), left[1]);

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
        let border_color = if is_active {
            TOPUP_CARD_ACTIVE_BG
        } else if is_selected {
            TOPUP_CARD_INACTIVE_SELECTED_BG
        } else {
            Color::DarkGray
        };
        let title_color = if is_active {
            TOPUP_CARD_ACTIVE_BG
        } else {
            Color::White
        };
        let subtitle_color = if is_selected {
            Color::White
        } else {
            Color::Gray
        };
        let marker_color = if is_selected {
            TOPUP_CARD_ACTIVE_BG
        } else {
            TOPUP_CARD_BG
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(TOPUP_CARD_BG));
        let card = Paragraph::new(vec![
            Line::from(Span::styled(
                option.title(),
                Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled(
                    if is_active { "● " } else { "  " },
                    Style::default().fg(marker_color),
                ),
                Span::styled(option.subtitle(), Style::default().fg(subtitle_color)),
            ]),
        ])
        .block(block);
        frame.render_widget(card, option_chunks[chunk_idx]);
    }

    let active = options[selected];
    match active {
        TopupOption::TransferFromExistingAccount => {
            render_qr_detail(frame, right[1], pubkey, account_name, amount_pos, blink)
        }
        TopupOption::BuyStablecoins => render_buy_stablecoins_detail(
            frame,
            right[1],
            pubkey,
            payment_method,
            onramp,
            onramp_notice,
            onramp_error,
        ),
    }

    render_topup_controls(
        frame,
        chunks[1],
        active,
        status,
        payment_method,
        onramp.is_some(),
    );
}

fn render_qr_detail(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    account_name: &str,
    amount_pos: usize,
    blink: Option<&BlinkState>,
) {
    // Ensure the entire detail area has a dark background.
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );

    // Reserve slider space first so the QR gets whatever remains.
    let split = Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).split(area);

    // Compute QR size to get the exact area it occupies, even during blink.
    let url = solana_pay_url(pubkey, amount_pos);
    let qr = render_qr(&url, split[0].width, split[0].height)
        .ok()
        .flatten()
        .unwrap_or_else(unavailable_qr);
    let qr_area = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(qr.height),
        Constraint::Min(0),
    ])
    .split(split[0]);

    let h_pad = qr_area[1].width.saturating_sub(qr.width) / 2;
    let v_pad = qr_area[1].height.saturating_sub(qr.height) / 2;
    let qr_rect = Rect {
        x: qr_area[1].x + h_pad,
        y: qr_area[1].y + v_pad,
        width: qr.width.min(qr_area[1].width),
        height: qr.height.min(qr_area[1].height),
    };

    if let Some(b) = blink {
        render_success_checkmark(frame, qr_rect, b.visible);
    } else {
        frame.render_widget(
            Paragraph::new(qr.lines).style(Style::default().bg(TOPUP_MAIN_BG).fg(Color::White)),
            qr_rect,
        );
    }

    let amount_str = if amount_pos == 0 {
        "any".to_string()
    } else {
        format!("${:.0}", amount_pos as f64 * TOPUP_STEP_USDC)
    };
    let title = Line::from(vec![
        Span::raw(" Send "),
        Span::styled(
            amount_str,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" to account "),
        Span::styled(
            format!("@{account_name}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    render_slider_box(
        frame,
        split[1],
        title,
        amount_pos,
        TOPUP_MAX_STEPS,
        &[(0, "any"), (5, "$5"), (10, "$10"), (25, "$25")],
        false,
    );
}

fn solana_pay_url(pubkey: &str, amount_pos: usize) -> String {
    if amount_pos > 0 {
        let amount = (amount_pos as f64) * TOPUP_STEP_USDC;
        format!("solana:{pubkey}?amount={amount}&spl-token={USDC_MINT}")
    } else {
        format!("solana:{pubkey}?spl-token={USDC_MINT}")
    }
}

fn render_buy_stablecoins_detail(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    payment_method: OnrampPaymentMethod,
    onramp: Option<&OnrampSession>,
    onramp_notice: Option<&str>,
    onramp_error: Option<&str>,
) {
    frame.render_widget(
        Block::default().style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Buy stablecoins via MoonPay",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    match onramp {
        None => {
            lines.push(Line::from(Span::styled(
                "Pay with card, Apple Pay, or bank.",
                Style::default().fg(Color::Gray),
            )));
            lines.push(Line::from(Span::styled(
                "USDC will be sent to:",
                Style::default().fg(Color::Gray),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {pubkey}"),
                Style::default().fg(Color::White),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "What payment method would you like to use to onramp?",
                Style::default().fg(Color::Gray),
            )));
            lines.push(Line::from(vec![
                Span::styled(
                    if payment_method == OnrampPaymentMethod::Paypal {
                        "● "
                    } else {
                        "○ "
                    },
                    Style::default().fg(if payment_method == OnrampPaymentMethod::Paypal {
                        Color::Green
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    OnrampPaymentMethod::Paypal.title(),
                    Style::default().fg(if payment_method == OnrampPaymentMethod::Paypal {
                        Color::White
                    } else {
                        Color::Gray
                    }),
                ),
                Span::raw("   "),
                Span::styled(
                    if payment_method == OnrampPaymentMethod::Venmo {
                        "● "
                    } else {
                        "○ "
                    },
                    Style::default().fg(if payment_method == OnrampPaymentMethod::Venmo {
                        Color::Green
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::styled(
                    OnrampPaymentMethod::Venmo.title(),
                    Style::default().fg(if payment_method == OnrampPaymentMethod::Venmo {
                        Color::White
                    } else {
                        Color::Gray
                    }),
                ),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("Press ", Style::default().fg(Color::Gray)),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " to copy this wallet address and open MoonPay.",
                    Style::default().fg(Color::Gray),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                format!(
                    "We'll copy the address first, then launch MoonPay with {}.",
                    payment_method.title()
                ),
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(
                "When MoonPay asks which wallet to fund, paste your copied pubkey.",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(session) => {
            let elapsed = session.started_at.elapsed().as_secs();
            lines.push(Line::from(Span::styled(
                "MoonPay opened in your browser.",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("Status:  ", Style::default().fg(Color::Gray)),
                Span::styled("waiting for payment…", Style::default().fg(Color::Yellow)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Elapsed: ", Style::default().fg(Color::Gray)),
                Span::styled(format!("{elapsed}s"), Style::default().fg(Color::White)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Method:  ", Style::default().fg(Color::Gray)),
                Span::styled(
                    session.payment_method.title(),
                    Style::default().fg(Color::White),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Tx ID:   ", Style::default().fg(Color::Gray)),
                Span::styled(
                    session.external_id.clone(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "r",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" reopen browser  ·  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "Esc",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" abort", Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    if let Some(notice) = onramp_notice {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            notice.to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    if let Some(err) = onramp_error {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("Last attempt failed: {err}"),
            Style::default().fg(Color::Red),
        )));
        lines.push(Line::from(Span::styled(
            "Press Enter to retry.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(TOPUP_MAIN_BG)),
        area,
    );
}

fn unavailable_qr() -> RenderedQr {
    let lines = vec![Line::from(Span::styled(
        "QR unavailable",
        Style::default().fg(Color::DarkGray),
    ))];
    RenderedQr {
        width: lines.first().map(Line::width).unwrap_or(0) as u16,
        height: lines.len() as u16,
        lines,
    }
}

fn render_qr(
    data: &str,
    max_width: u16,
    max_height: u16,
) -> Result<Option<RenderedQr>, qrcode::types::QrError> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::L)?;
    let modules = code.width();
    let Some((module_cols, module_subrows)) =
        choose_qr_module_cells(modules, max_width, max_height)
    else {
        return Ok(None);
    };

    let scaled_rows = modules * module_subrows;
    let mut lines = Vec::with_capacity(scaled_rows.div_ceil(2));
    for top_subrow in (0..scaled_rows).step_by(2) {
        let mut spans = Vec::with_capacity(modules);
        for x in 0..modules {
            let top_dark = qr_subrow_dark(&code, x, top_subrow, module_subrows);
            let bottom_dark = qr_subrow_dark(&code, x, top_subrow + 1, module_subrows);
            spans.push(render_qr_half_block(top_dark, bottom_dark, module_cols));
        }

        lines.push(Line::from(spans));
    }
    let width = lines.first().map(Line::width).unwrap_or(0) as u16;
    let height = lines.len() as u16;

    Ok(Some(RenderedQr {
        lines,
        width,
        height,
    }))
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

fn render_topup_controls(
    frame: &mut ratatui::Frame,
    area: Rect,
    active: TopupOption,
    status: &PollStatus,
    payment_method: OnrampPaymentMethod,
    onramp_active: bool,
) {
    let mut spans = match active {
        TopupOption::TransferFromExistingAccount => vec![
            Span::styled("↑ ↓", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" move  │  ", Style::default().dim()),
            Span::styled("← →", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" amount  │  ", Style::default().dim()),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::styled(" skip", Style::default().dim()),
        ],
        TopupOption::BuyStablecoins if onramp_active => vec![
            Span::styled("↑ ↓", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" move  │  ", Style::default().dim()),
            Span::styled("r", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" reopen  │  ", Style::default().dim()),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::styled(" abort", Style::default().dim()),
        ],
        TopupOption::BuyStablecoins => vec![
            Span::styled("↑ ↓", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" move  │  ", Style::default().dim()),
            Span::styled("← →", Style::default().fg(Color::Cyan).bold()),
            Span::styled(
                format!(" {}  │  ", payment_method.title()),
                Style::default().dim(),
            ),
            Span::styled("Enter", Style::default().fg(Color::Green).bold()),
            Span::styled(" copy + open  │  ", Style::default().dim()),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::styled(" skip", Style::default().dim()),
        ],
    };

    let status_spans = match status {
        PollStatus::RpcUnavailable => vec![Span::styled(
            "offline",
            Style::default().fg(Color::Red).bold(),
        )],
        PollStatus::Waiting { secs_left } => vec![
            Span::styled("waiting ", Style::default().fg(Color::Yellow).bold()),
            Span::styled(
                format!("{secs_left}s…"),
                Style::default().fg(Color::Yellow).bold(),
            ),
        ],
        PollStatus::Checking { spinner_idx } => {
            const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            vec![
                Span::styled(
                    SPINNER[spinner_idx % SPINNER.len()],
                    Style::default().fg(Color::Green).bold(),
                ),
                Span::styled(" checking…", Style::default().fg(Color::Green).bold()),
            ]
        }
        PollStatus::Countdown { secs_left } => vec![
            Span::styled(
                format!("{secs_left}s"),
                Style::default().fg(Color::DarkGray).bold(),
            ),
            Span::styled("  ", Style::default()),
            Span::styled("R", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" refresh", Style::default().dim()),
        ],
    };

    let controls_width: usize = spans.iter().map(|span| span.content.len()).sum();
    let status_width: usize = status_spans.iter().map(|span| span.content.len()).sum();
    let total_width = controls_width.saturating_add(status_width);
    let gap = (area.width as usize).saturating_sub(total_width);
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
    eprintln!("  2. Buy funds with MoonPay: {DEFAULT_ONRAMP_URL}");
}

/// Build a sanitized `externalTransactionId` (UUID v4 with the `pay-` prefix).
fn new_external_id() -> String {
    format!("pay-{}", uuid::Uuid::new_v4())
}

fn resolve_moonpay_api_key() -> Result<String, String> {
    match std::env::var("PAY_MOONPAY_API_KEY") {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) => Err("PAY_MOONPAY_API_KEY is empty.".to_string()),
        Err(_) => Err("PAY_MOONPAY_API_KEY is not set.".to_string()),
    }
}

fn build_onramp_redirect_url(host: &str) -> String {
    format!("{host}/onramp/done")
}

/// Compose the direct MoonPay checkout URL we open in the browser.
fn build_onramp_url(
    host: &str,
    pubkey: &str,
    external_id: &str,
    api_key: &str,
    payment_method: OnrampPaymentMethod,
) -> String {
    let redirect_url = build_onramp_redirect_url(host);
    format!(
        "{MOONPAY_BUY_URL}?apiKey={}&currencyCode=usdc_sol&walletAddress={}&baseCurrencyAmount=20&externalTransactionId={}&redirectURL={}&paymentMethod={}",
        urlencoding::encode(api_key),
        urlencoding::encode(pubkey),
        urlencoding::encode(external_id),
        urlencoding::encode(&redirect_url),
        urlencoding::encode(payment_method.query_value()),
    )
}

/// Launch a fresh MoonPay session: open the browser and start a status poller.
fn launch_onramp_session(
    onramp_host: &str,
    pubkey: &str,
    payment_method: OnrampPaymentMethod,
    updates: &mpsc::Sender<OnrampUpdate>,
) -> Result<OnrampSession, String> {
    let host = onramp_host.trim_end_matches('/').to_string();
    let api_key = resolve_moonpay_api_key()?;
    let external_id = new_external_id();
    let url = build_onramp_url(&host, pubkey, &external_id, &api_key, payment_method);
    open_url(&url).map_err(|err| format!("failed to open MoonPay: {err}"))?;
    spawn_onramp_poller(external_id.clone(), api_key, updates.clone());
    Ok(OnrampSession {
        external_id,
        url,
        payment_method,
        started_at: Instant::now(),
    })
}

#[derive(Debug, serde::Deserialize)]
struct MoonpayCurrency {
    code: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct MoonpayTransaction {
    status: String,
    #[serde(rename = "failureReason")]
    failure_reason: Option<String>,
    #[serde(rename = "cryptoTransactionId")]
    crypto_transaction_id: Option<String>,
    #[serde(rename = "quoteCurrencyAmount")]
    quote_currency_amount: Option<serde_json::Number>,
    currency: Option<MoonpayCurrency>,
}

fn moonpay_amount_string(amount: &Option<serde_json::Number>) -> Option<String> {
    amount.as_ref().map(ToString::to_string)
}

fn interpret_moonpay_transactions(
    transactions: &[MoonpayTransaction],
) -> Result<Option<OnrampUpdate>, String> {
    match transactions {
        [] => Ok(None),
        [transaction] => match transaction.status.as_str() {
            "completed" => Ok(Some(OnrampUpdate::Completed {
                tx_hash: transaction.crypto_transaction_id.clone(),
                crypto_amount: moonpay_amount_string(&transaction.quote_currency_amount),
                crypto_currency: transaction
                    .currency
                    .as_ref()
                    .and_then(|currency| currency.code.clone()),
            })),
            "failed" => Ok(Some(OnrampUpdate::Failed {
                reason: transaction
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "unknown reason".into()),
            })),
            _ => Ok(None),
        },
        _ => {
            Err("multiple MoonPay transactions matched this top-up; refusing to guess.".to_string())
        }
    }
}

/// Background thread that polls MoonPay's external-id lookup endpoint every
/// [`ONRAMP_POLL_INTERVAL`] until it sees a terminal status, exits, or hits
/// [`ONRAMP_POLL_TIMEOUT`].
fn spawn_onramp_poller(external_id: String, api_key: String, tx: mpsc::Sender<OnrampUpdate>) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Runtime::new() else {
            return;
        };
        let url = format!(
            "{MOONPAY_EXTERNAL_TX_ENDPOINT}/{}?apiKey={}",
            urlencoding::encode(&external_id),
            urlencoding::encode(&api_key)
        );
        let client = reqwest::Client::new();
        let started = Instant::now();

        rt.block_on(async {
            loop {
                if started.elapsed() > ONRAMP_POLL_TIMEOUT {
                    return;
                }
                if let Ok(resp) = client.get(&url).send().await {
                    match resp.status() {
                        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                            let _ = tx.send(OnrampUpdate::Failed {
                                reason: "MoonPay status polling was unauthorized; check PAY_MOONPAY_API_KEY."
                                    .to_string(),
                            });
                            return;
                        }
                        status if status.is_success() => {
                            if let Ok(body) = resp.json::<Vec<MoonpayTransaction>>().await {
                                match interpret_moonpay_transactions(&body) {
                                    Ok(Some(update)) => {
                                        let is_terminal = matches!(
                                            update,
                                            OnrampUpdate::Completed { .. } | OnrampUpdate::Failed { .. }
                                        );
                                        let _ = tx.send(update);
                                        if is_terminal {
                                            return;
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(reason) => {
                                        let _ = tx.send(OnrampUpdate::Failed { reason });
                                        return;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                tokio::time::sleep(ONRAMP_POLL_INTERVAL).await;
            }
        });
    });
}

/// Convert a MoonPay completion (amount + currency code) into a [`ReceivedFunds`]
/// shape so the existing post-topup formatting logic in `account/new.rs` can
/// render the same “Funded!” summary regardless of which top-up path the user
/// took.
fn synthesize_received_funds(amount: &Option<String>, currency: &Option<String>) -> ReceivedFunds {
    let ui_amount = amount
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let symbol: Option<&'static str> = match currency.as_deref() {
        Some("usdc_sol") | Some("usdc") => Some("USDC"),
        Some("sol") => None, // surface as SOL via lamports below
        _ => None,
    };
    if currency.as_deref() == Some("sol") {
        let lamports = (ui_amount * 1_000_000_000.0) as u64;
        return ReceivedFunds {
            sol_lamports: lamports,
            tokens: Vec::new(),
        };
    }
    ReceivedFunds {
        sol_lamports: 0,
        tokens: vec![ReceivedToken {
            mint: USDC_MINT.to_string(),
            ui_amount,
            symbol,
        }],
    }
}

fn open_url(url: &str) -> io::Result<()> {
    webbrowser::open(url).map_err(io::Error::other)
}

fn pipe_to_command(program: &str, args: &[&str], text: &str) -> io::Result<()> {
    let mut child = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{program} exited with status {status}"
        )))
    }
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        return pipe_to_command("pbcopy", &[], text);
    }

    #[cfg(target_os = "windows")]
    {
        return pipe_to_command("cmd", &["/C", "clip"], text);
    }

    #[cfg(target_os = "linux")]
    {
        let commands: &[(&str, &[&str])] = &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ];
        let mut last_err = None;
        for (program, args) in commands {
            match pipe_to_command(program, args, text) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => last_err = Some(err),
            }
        }
        return Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no clipboard command available")
        }));
    }

    #[allow(unreachable_code)]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clipboard copy is not supported on this platform",
    ))
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    tool: ToolKind,
    account_name: &str,
) -> io::Result<SessionSetup> {
    let mut budget_pos: usize = 2; // $1.00
    let mut expiry_pos: usize = 3; // 1h
    let mut focus = Focus::Budget;

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            render_session_setup(
                frame,
                area,
                budget_pos,
                expiry_pos,
                &focus,
                tool,
                account_name,
            );
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
    account_name: &str,
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

    render_left_panel(
        frame,
        columns[0],
        budget_pos,
        expiry_pos,
        focus,
        account_name,
    );
    render_card_panel(frame, columns[1], budget_pos, expiry_pos, tool);
    render_controls(frame, chunks[1]);
}

fn render_left_panel(
    frame: &mut ratatui::Frame,
    area: Rect,
    budget_pos: usize,
    expiry_pos: usize,
    focus: &Focus,
    account_name: &str,
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

    frame.render_widget(Paragraph::new(solana_logo("")).centered(), content[1]);

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

    render_budget_box(
        frame,
        center(content[3]),
        budget_pos,
        max_w,
        focus,
        account_name,
    );
    render_expiry_box(frame, center(content[5]), expiry_pos, focus);
}

fn render_budget_box(
    frame: &mut ratatui::Frame,
    area: Rect,
    position: usize,
    _box_width: u16,
    focus: &Focus,
    account_name: &str,
) {
    let is_yolo = position >= MAX_STEPS;
    let amount_str = if is_yolo {
        "YOLO".to_string()
    } else {
        format!("${:.0}", position as f64 * 0.5)
    };
    let title = Line::from(vec![
        Span::raw(" Send "),
        Span::styled(
            amount_str,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" to account "),
        Span::styled(
            format!("@{account_name}"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);
    render_slider_box(
        frame,
        area,
        title,
        position,
        MAX_STEPS,
        &[
            (0, "$0"),
            (10, "$5"),
            (20, "$10"),
            (30, "$15"),
            (31, "YOLO"),
        ],
        *focus == Focus::Budget,
    );
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
        ToolKind::Codex => {
            let mut lines = vec![Line::default()];
            lines.extend(solana_logo("  "));
            lines.push(Line::from(Span::styled(
                "  codex",
                Style::default().fg(Color::DarkGray),
            )));
            lines
        }
        _ => {
            let tool_label = match tool {
                ToolKind::Curl => "curl",
                ToolKind::Wget => "wget",
                ToolKind::Http => "http",
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

fn solana_logo(prefix: &'static str) -> Vec<Line<'static>> {
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
        let label_start = label_center
            .saturating_sub(label_width / 2)
            .min(bar_width.saturating_sub(label_width));

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
fn render_slider_box<'a>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const SAMPLE_SOLANA_PAY_URL: &str = "solana:11111111111111111111111111111111?amount=5&spl-token=\
         EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var(key).ok();
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
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
    fn build_onramp_redirect_url_targets_done_page() {
        assert_eq!(
            build_onramp_redirect_url("https://pay.sh"),
            "https://pay.sh/onramp/done"
        );
    }

    #[test]
    fn build_onramp_url_targets_moonpay_with_fixed_params() {
        let url = build_onramp_url(
            "https://pay.sh",
            "wallet123",
            "pay-abc",
            "moonpay-key",
            OnrampPaymentMethod::Paypal,
        );
        let parsed = reqwest::Url::parse(&url).expect("MoonPay URL should parse");
        let query = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert!(parsed.as_str().starts_with(MOONPAY_BUY_URL));
        assert_eq!(query.get("apiKey"), Some(&"moonpay-key".into()));
        assert_eq!(query.get("currencyCode"), Some(&"usdc_sol".into()));
        assert_eq!(query.get("walletAddress"), Some(&"wallet123".into()));
        assert_eq!(query.get("baseCurrencyAmount"), Some(&"20".into()));
        assert_eq!(query.get("externalTransactionId"), Some(&"pay-abc".into()));
        assert_eq!(
            query.get("redirectURL"),
            Some(&"https://pay.sh/onramp/done".into())
        );
        assert_eq!(query.get("paymentMethod"), Some(&"paypal".into()));
        assert!(!query.contains_key("account"));
    }

    #[test]
    fn interpret_moonpay_transactions_keeps_polling_for_empty_array() {
        let result = interpret_moonpay_transactions(&[]).expect("empty array should be valid");

        assert_eq!(result, None);
    }

    #[test]
    fn interpret_moonpay_transactions_maps_completed_transaction() {
        let body: Vec<MoonpayTransaction> = serde_json::from_str(
            r#"[
                {
                    "status": "completed",
                    "cryptoTransactionId": "tx-123",
                    "quoteCurrencyAmount": 19.95,
                    "currency": { "code": "usdc_sol" }
                }
            ]"#,
        )
        .expect("MoonPay payload should parse");

        let result = interpret_moonpay_transactions(&body).expect("completed payload should parse");

        assert_eq!(
            result,
            Some(OnrampUpdate::Completed {
                tx_hash: Some("tx-123".to_string()),
                crypto_amount: Some("19.95".to_string()),
                crypto_currency: Some("usdc_sol".to_string()),
            })
        );
    }

    #[test]
    fn interpret_moonpay_transactions_maps_failed_transaction() {
        let body: Vec<MoonpayTransaction> = serde_json::from_str(
            r#"[
                {
                    "status": "failed",
                    "failureReason": "card_declined"
                }
            ]"#,
        )
        .expect("MoonPay payload should parse");

        let result = interpret_moonpay_transactions(&body).expect("failed payload should parse");

        assert_eq!(
            result,
            Some(OnrampUpdate::Failed {
                reason: "card_declined".to_string(),
            })
        );
    }

    #[test]
    fn interpret_moonpay_transactions_ignores_inflight_transaction() {
        let body: Vec<MoonpayTransaction> = serde_json::from_str(
            r#"[
                {
                    "status": "waitingPayment",
                    "quoteCurrencyAmount": 20
                }
            ]"#,
        )
        .expect("MoonPay payload should parse");

        let result = interpret_moonpay_transactions(&body).expect("in-flight payload should parse");

        assert_eq!(result, None);
    }

    #[test]
    fn interpret_moonpay_transactions_rejects_multiple_matches() {
        let body: Vec<MoonpayTransaction> = serde_json::from_str(
            r#"[
                { "status": "pending" },
                { "status": "completed" }
            ]"#,
        )
        .expect("MoonPay payload should parse");

        let err = interpret_moonpay_transactions(&body).expect_err("multiple matches should fail");

        assert!(err.contains("multiple MoonPay transactions matched"));
    }

    #[test]
    fn synthesize_received_funds_maps_usdc_amounts() {
        let received =
            synthesize_received_funds(&Some("19.95".to_string()), &Some("usdc_sol".to_string()));

        assert_eq!(received.sol_lamports, 0);
        assert_eq!(received.tokens.len(), 1);
        assert_eq!(received.tokens[0].mint, USDC_MINT);
        assert_eq!(received.tokens[0].ui_amount, 19.95);
        assert_eq!(received.tokens[0].symbol, Some("USDC"));
    }

    #[test]
    #[serial]
    fn launch_onramp_session_requires_api_key() {
        let _api_key = EnvVarGuard::remove("PAY_MOONPAY_API_KEY");
        let (tx, _rx) = mpsc::channel();

        let err = launch_onramp_session(
            "https://pay.sh",
            "wallet123",
            OnrampPaymentMethod::Paypal,
            &tx,
        )
        .expect_err("missing API key should fail");

        assert!(err.contains("PAY_MOONPAY_API_KEY"));
    }

    #[test]
    #[serial]
    fn resolve_moonpay_api_key_reads_env() {
        let _api_key = EnvVarGuard::set("PAY_MOONPAY_API_KEY", "moonpay-key");

        let value = resolve_moonpay_api_key().expect("env API key should resolve");

        assert_eq!(value, "moonpay-key");
    }
}
