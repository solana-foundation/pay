//! Interactive TUI for configuring a payment session.
//!
//! Shown before making requests when automatic payment is not pre-approved.
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
use pay_core::client::balance::{AccountBalances, ReceivedFunds};
use qrcode::{Color as QrColor, QrCode, Version as QrVersion};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

/// Wait this long after the cycle starts (TUI open or QR recompute) before
/// the first balance check. Gives the user time to scan & broadcast.
const POLL_DELAY: Duration = Duration::from_secs(5);
/// Minimum spacing between consecutive balance checks once polling begins.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Total polling window for a single cycle. After this, polling stops and
/// the user has to press R to start a new cycle.
const POLL_WINDOW: Duration = Duration::from_secs(300);
/// Safety timeout: if a spawned check thread doesn't report back within this
/// duration we assume it's wedged and clear the `checking` flag so the next
/// tick can dispatch a new one.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Result from the polling thread: what changed + current totals.
#[derive(Debug, Clone)]
struct TopupDetected {
    received: ReceivedFunds,
    /// On-chain transaction hash for the funding transfer, when known.
    tx_hash: Option<String>,
}

/// Message sent from the spawned stablecoin-check thread back to the TUI loop.
#[derive(Debug)]
enum CheckMsg {
    /// First successful balance fetch — promotes from "no baseline" to
    /// "polling for diffs". The previous baseline (if any) was unhealthy
    /// (`tokens_unavailable`) and is replaced.
    BaselineEstablished(AccountBalances),
    /// Diff against the existing baseline detected incoming funds.
    Detected(TopupDetected),
    /// Check finished without anything to report — clear `checking`.
    Done,
}

struct RenderedQr {
    lines: Vec<Line<'static>>,
    width: u16,
    height: u16,
}

/// What the status line should show.
///
/// All non-stalled variants render the same way in the bottom status bar
/// (`<spinner> waiting for transfer…`), but we keep them as distinct cases
/// so the state machine remains introspectable and unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum PollStatus {
    /// Cycle just started — waiting POLL_DELAY before the first check.
    Waiting { secs_left: u64, spinner_idx: usize },
    /// A balance check is currently in flight.
    Checking { spinner_idx: usize },
    /// Idle between 1s polls. `secs_left_in_window` is how many seconds
    /// remain before the cycle stalls.
    Polling {
        secs_left_in_window: u64,
        spinner_idx: usize,
    },
    /// Cycle window (POLL_WINDOW) elapsed without detecting a topup.
    /// Triggers the yellow "press R" banner; polling halts until reset.
    Stalled,
}

/// Pure state machine for the topup stablecoin-polling cadence. Held inside
/// `run_topup_flow` and consulted each tick to decide whether to spawn a
/// new check, what status to render, etc. All time-dependent methods take
/// `now: Instant` so they're deterministic in tests.
struct PollState {
    /// When the current polling cycle started (TUI open, QR recompute, or
    /// R press). Resets the delay/window timers.
    cycle_started_at: Instant,
    /// When the most recent check thread was spawned. Drives both the
    /// 1s POLL_INTERVAL throttle and the CHECK_TIMEOUT stuck-thread reset.
    last_check_started_at: Option<Instant>,
    /// Reference stablecoin balances for diff_received. `None` means no
    /// successful fetch yet — the next check will establish baseline rather
    /// than diff.
    /// `Some(b)` with `b.tokens_unavailable` is treated as no baseline so
    /// pay-api flakes during init don't poison the diff.
    baseline: Option<AccountBalances>,
    /// True while a spawned check thread is still in flight.
    checking: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum PollDecision {
    /// Don't spawn anything this tick.
    Idle,
    /// Spawn a stablecoin balance-check thread now.
    SpawnCheck,
}

impl PollState {
    fn new(now: Instant, baseline: Option<AccountBalances>) -> Self {
        Self {
            cycle_started_at: now,
            last_check_started_at: None,
            baseline: baseline.filter(|b| !b.tokens_unavailable),
            checking: false,
        }
    }

    /// Restart the cycle: reset the 5s delay and the 5min window. Called
    /// when the QR is recomputed (amount slider moved) or the user presses
    /// R. Baseline is preserved across resets so we keep diffing against
    /// the original snapshot.
    fn reset_cycle(&mut self, now: Instant) {
        self.cycle_started_at = now;
        self.last_check_started_at = None;
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn has_healthy_baseline(&self) -> bool {
        self.baseline
            .as_ref()
            .is_some_and(|b| !b.tokens_unavailable)
    }

    fn cycle_elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.cycle_started_at)
    }

    fn past_delay(&self, now: Instant) -> bool {
        self.cycle_elapsed(now) >= POLL_DELAY
    }

    fn past_window(&self, now: Instant) -> bool {
        self.cycle_elapsed(now) >= POLL_WINDOW
    }

    /// Whether to spawn a check this tick.
    fn decide(&self, now: Instant) -> PollDecision {
        if self.checking || self.past_window(now) {
            return PollDecision::Idle;
        }

        let interval_elapsed = match self.last_check_started_at {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= POLL_INTERVAL,
        };
        if !interval_elapsed {
            return PollDecision::Idle;
        }

        if self.baseline.is_none() || self.past_delay(now) {
            PollDecision::SpawnCheck
        } else {
            PollDecision::Idle
        }
    }

    fn on_check_started(&mut self, now: Instant) {
        self.checking = true;
        self.last_check_started_at = Some(now);
    }

    fn on_check_done(&mut self) {
        self.checking = false;
    }

    fn on_baseline_established(&mut self, b: AccountBalances) {
        // Only accept healthy baselines — defensive guard.
        if !b.tokens_unavailable {
            self.baseline = Some(b);
        }
        self.checking = false;
    }

    /// If a check thread spawned more than CHECK_TIMEOUT ago and never sent
    /// a result, free the `checking` slot so the next tick can retry.
    fn clear_stuck_check(&mut self, now: Instant) {
        if self.checking
            && self
                .last_check_started_at
                .is_some_and(|t| now.saturating_duration_since(t) >= CHECK_TIMEOUT)
        {
            self.checking = false;
        }
    }

    fn status(&self, now: Instant) -> PollStatus {
        if self.past_window(now) {
            return PollStatus::Stalled;
        }
        let spinner_idx = (self.cycle_elapsed(now).as_millis() / 80) as usize;
        if !self.past_delay(now) {
            let secs_left = POLL_DELAY.saturating_sub(self.cycle_elapsed(now)).as_secs();
            return PollStatus::Waiting {
                secs_left,
                spinner_idx,
            };
        }
        if self.checking {
            return PollStatus::Checking { spinner_idx };
        }
        let secs_left_in_window = POLL_WINDOW
            .saturating_sub(self.cycle_elapsed(now))
            .as_secs();
        PollStatus::Polling {
            secs_left_in_window,
            spinner_idx,
        }
    }
}

/// Slider range: $0.00 to $15.00 in $0.50 increments = 30 steps, + 1 no-cap step = 31
const MAX_STEPS: usize = 31;
const STEP_AMOUNT: u64 = 500_000; // 0.50 USDC in base units (6 decimals)

/// Topup amount slider: 0 = any amount, 1-25 = $1 to $25 in $1 steps
const TOPUP_MAX_STEPS: usize = 25;
const TOPUP_STEP_USDC: f64 = 1.0;
const TOPUP_AMOUNT_LABEL_WIDTH: usize = 3;
const TOPUP_SLIDER_CELL: &str = "▐";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// Production onramp host — same gateway that serves the pay-api, just a
/// different path (`/v1/onramp/start`). Reuses `pay_core::balance::DEFAULT_PAY_API_URL`
/// so the two stay in sync if the host ever moves.
const DEFAULT_ONRAMP_HOST: &str = pay_core::client::balance::DEFAULT_PAY_API_URL;

const CARD_WIDTH: u16 = 36;
const CARD_BG: Color = Color::Rgb(35, 40, 50);
const TOPUP_SIDEBAR_BG: Color = Color::Rgb(24, 24, 27);
const TOPUP_MAIN_BG: Color = Color::Rgb(9, 9, 11);
const TOPUP_CARD_BG: Color = Color::Rgb(39, 39, 42);
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

    /// Solana-brand background color for the option card when active.
    /// Picked from the logo palette directly above the cards.
    fn brand_color(self) -> Color {
        match self {
            Self::TransferFromExistingAccount => SOLANA_PURPLE,
            Self::BuyStablecoins => SOLANA_GREEN,
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
    ApplePay,
}

impl OnrampPaymentMethod {
    fn default() -> Self {
        Self::Paypal
    }

    fn all() -> [Self; 3] {
        [Self::Paypal, Self::Venmo, Self::ApplePay]
    }

    fn title(self) -> &'static str {
        match self {
            Self::Paypal => "PayPal",
            Self::Venmo => "Venmo",
            Self::ApplePay => "Apple Pay",
        }
    }

    fn query_value(self) -> &'static str {
        match self {
            Self::Paypal => "paypal",
            Self::Venmo => "venmo",
            Self::ApplePay => "apple_pay",
        }
    }

    /// Brand-color background used when this option is selected.
    fn brand_color(self) -> Color {
        match self {
            // Approximations of each brand's primary color, dimmed slightly
            // so they read OK on the dark TUI background.
            Self::Paypal => Color::Rgb(255, 196, 57),
            Self::Venmo => Color::Rgb(0, 140, 255),
            Self::ApplePay => Color::White,
        }
    }

    /// Foreground colour used for the title when the button is selected —
    /// dark on light brand backgrounds (Apple Pay), white otherwise.
    fn brand_text_color(self) -> Color {
        match self {
            Self::Paypal | Self::Venmo => Color::White,
            Self::ApplePay => Color::Black,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Paypal => Self::ApplePay,
            Self::Venmo => Self::Paypal,
            Self::ApplePay => Self::Venmo,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Paypal => Self::Venmo,
            Self::Venmo => Self::ApplePay,
            Self::ApplePay => Self::Paypal,
        }
    }
}

/// Resolve the redirect host from `PAY_ONRAMP_HOST`, falling back to
/// `https://api.gateway-402.com`. Trailing slashes are stripped so callers can `format!`
/// without double-slash hazards.
fn resolve_onramp_host() -> String {
    let raw = std::env::var("PAY_ONRAMP_HOST").unwrap_or_else(|_| DEFAULT_ONRAMP_HOST.to_string());
    raw.trim_end_matches('/').to_string()
}

/// Run the interactive top-up TUI for an account.
///
/// Presents two options to the user:
///
/// 1. **Buy stablecoins** (default) — copies the destination wallet address to
///    the clipboard, shows a brief launch animation, then opens the
///    `/v1/onramp/start` endpoint on the gateway in the user's browser.
///    `PAY_ONRAMP_HOST` controls the host, defaulting to
///    `DEFAULT_ONRAMP_HOST` (the production gateway).
/// 2. **Top-up from mobile wallet** — renders a USDC Solana Pay QR code that
///    any Solana wallet can scan, while polling pay-api for incoming
///    stablecoin balance changes against `pubkey`.
///
/// Both paths rely on stablecoin balance polling: a USDC/stablecoin increase
/// will end the flow with `Ok(Some(_))`. The user can
/// dismiss the TUI at any time with `Esc`/`q`/`Ctrl-C`, which yields
/// `Ok(None)`.
///
/// When stderr is not a TTY (e.g. CI, piped output), this falls back to
/// printing static top-up instructions and returns `Ok(None)` immediately.
///
/// # Parameters
/// - `pubkey`: base58 destination address shown in the QR code and threaded
///   into the onramp URL as the locked `walletAddress`.
/// - `rpc_url`: used only to infer which network the pay-api stablecoin
///   balance poller should query.
/// - `account_name`: human-readable account label rendered in the TUI.
///
/// # Returns
/// - `Ok(Some(TopupCompletion))` if funds landed. The completion carries the
///   detected [`ReceivedFunds`] and an optional tx hash when known.
/// - `Ok(None)` if the user dismissed without funding.
/// - `Err(_)` if the terminal could not be entered or restored.
pub fn run_topup_flow(
    pubkey: &str,
    rpc_url: &str,
    account_name: &str,
) -> pay_core::Result<Option<TopupCompletion>> {
    let onramp_host = resolve_onramp_host();
    if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        print_topup_instructions(pubkey, &onramp_host);
        return Ok(None);
    }

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
    /// On-chain tx hash, when known.
    pub tx_hash: Option<String>,
}

/// Spawn a background thread that fetches the current stablecoin balances and reports
/// the result back over `tx`:
///
/// - No baseline yet → forward `BaselineEstablished` on first healthy stablecoin fetch.
/// - Healthy baseline → diff against it; emit `Detected` on incoming stablecoin funds,
///   else `Done`.
/// - Pay-api or RPC unavailable → emit `Done` so `checking` clears for the
///   next tick to retry.
fn spawn_stablecoin_check(
    tx: &mpsc::Sender<CheckMsg>,
    baseline: Option<AccountBalances>,
    rpc_url: &str,
    pubkey: &str,
) {
    let rpc = rpc_url.to_string();
    let pk = pubkey.to_string();
    let tx = tx.clone();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(_) => {
                let _ = tx.send(CheckMsg::Done);
                return;
            }
        };
        let msg = match rt.block_on(pay_core::client::balance::get_stablecoin_balances(
            &rpc, &pk,
        )) {
            Ok(current) if current.tokens_unavailable => CheckMsg::Done,
            Ok(current) => match baseline {
                None => CheckMsg::BaselineEstablished(current),
                Some(b) => {
                    let received = current.diff_received(&b);
                    if received.has_any() {
                        CheckMsg::Detected(TopupDetected {
                            received,
                            tx_hash: None,
                        })
                    } else {
                        CheckMsg::Done
                    }
                }
            },
            Err(_) => CheckMsg::Done,
        };
        let _ = tx.send(msg);
    });
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
    let mut amount_pos: usize = 3; // default $3
    let mut last_amount_pos = amount_pos;

    // Active MoonPay session (set after the user hits Enter on "Buy stablecoins").
    let mut onramp: Option<OnrampSession> = None;
    let mut onramp_notice: Option<String> = None;
    let mut onramp_error: Option<String> = None;

    // Establish the first stablecoin baseline after the first TUI paint so
    // network latency never shows up as a blank pre-render pause.
    let mut poll = PollState::new(Instant::now(), None);
    let mut first_frame_drawn = false;

    // Channel for background balance checks.
    let (tx, rx) = mpsc::channel::<CheckMsg>();

    loop {
        let now = Instant::now();

        // Recompute QR cycle when the amount slider moves.
        if amount_pos != last_amount_pos {
            poll.reset_cycle(now);
            last_amount_pos = amount_pos;
        }

        // Spawn next check if cadence allows.
        if first_frame_drawn && poll.decide(now) == PollDecision::SpawnCheck {
            poll.on_check_started(now);
            spawn_stablecoin_check(&tx, poll.baseline.clone(), rpc_url, pubkey);
        }

        // Drain check results.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                CheckMsg::BaselineEstablished(b) => poll.on_baseline_established(b),
                CheckMsg::Detected(detected) => {
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
                CheckMsg::Done => poll.on_check_done(),
            }
        }

        // Safety reset for wedged check threads.
        poll.clear_stuck_check(now);

        let status = poll.status(now);

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
        first_frame_drawn = true;

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
                            match launch_onramp_session(onramp_host, pubkey, payment_method) {
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
                    // Reopen the gateway onramp URL. The server will create a
                    // fresh MoonPay checkout session for the new request.
                    if let Some(session) = onramp.as_ref() {
                        let _ = open_url(&session.url);
                    }
                }
                KeyCode::Char('r') | KeyCode::Char('R') => {
                    // Restart the cycle: resets the 5s delay and 5min window
                    // so polling resumes (or unstalls) regardless of state.
                    poll.reset_cycle(Instant::now());
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
    url: String,
    payment_method: OnrampPaymentMethod,
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
                &PollStatus::Polling {
                    secs_left_in_window: 0,
                    spinner_idx: 0,
                }, // status bar doesn't matter during blink
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
        // Reuse the Solana logo's brand colors so each option has a
        // distinct identity when active. No border — full background fill.
        let brand = option.brand_color();
        let bg = if is_active { brand } else { TOPUP_CARD_BG };
        let title_color = if is_active || is_selected {
            Color::White
        } else {
            Color::Gray
        };
        let block = Block::default().style(Style::default().bg(bg));
        // Card height is 3 — pad a blank line above the title so it lands
        // on the middle row.
        let card = Paragraph::new(vec![
            Line::default(),
            Line::from(Span::styled(
                option.title(),
                Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD),
            ))
            .centered(),
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
            account_name,
            status,
            payment_method,
            onramp,
            onramp_notice,
            onramp_error,
        ),
    }

    // Top banner — only rendered when the polling cycle has stalled; the
    // QR view's scan-instruction now lives in the slider title.
    if matches!(status, PollStatus::Stalled) {
        render_stalled_banner(frame, right[0]);
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

/// Build the human-readable scan-instruction text shown in the QR top
/// banner. Pure: separated from the render path so we can unit-test it.
fn scan_banner_text(account_name: &str, amount_pos: usize) -> String {
    if amount_pos == 0 {
        format!("Scan to send any amount of USDC to @{account_name}")
    } else {
        let amount = amount_pos as f64 * TOPUP_STEP_USDC;
        format!("Scan to send ${amount:.0} USDC to @{account_name}")
    }
}

/// Yellow notice rendered at the top of the right panel when the polling
/// window has elapsed. Asks the developer to press R to start a new cycle.
fn render_stalled_banner(frame: &mut ratatui::Frame, area: Rect) {
    let banner = Paragraph::new(Line::from(vec![
        Span::styled(
            "▲ idle 5m — press ",
            Style::default().fg(Color::Yellow).bold().bg(TOPUP_MAIN_BG),
        ),
        Span::styled(
            "R",
            Style::default().fg(Color::Cyan).bold().bg(TOPUP_MAIN_BG),
        ),
        Span::styled(
            " to refresh ",
            Style::default().fg(Color::Yellow).bold().bg(TOPUP_MAIN_BG),
        ),
    ]))
    .style(Style::default().bg(TOPUP_MAIN_BG))
    .right_aligned();
    frame.render_widget(banner, area);
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

    // Layout (top to bottom):
    //   slider        — borderless, sized 2× QR width, centered
    //   QR            — fills remaining vertical space, centered
    //   scan label    — white bold "Scan to send … to @account"
    //   dimmed helper — "Requires Solana wallet mobile application"
    //   dimmed address
    let split = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    // Render the QR first so we know its actual width — the slider sits
    // above and is sized to 2× that width.
    let url = solana_pay_url(pubkey, amount_pos);
    let qr = render_topup_qr(&url, pubkey, split[1].width, split[1].height)
        .ok()
        .flatten()
        .unwrap_or_else(unavailable_qr);
    let qr_area = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(qr.height),
        Constraint::Min(0),
    ])
    .split(split[1]);
    let h_pad = qr_area[1].width.saturating_sub(qr.width) / 2;
    let v_pad = qr_area[1].height.saturating_sub(qr.height) / 2;
    let qr_rect = Rect {
        x: qr_area[1].x + h_pad,
        y: qr_area[1].y + v_pad,
        width: qr.width.min(qr_area[1].width),
        height: qr.height.min(qr_area[1].height),
    };

    // ── slider (borderless, fixed width = 2× QR, centered) ─────────────
    let slider_width = qr.width.saturating_mul(2).min(split[0].width);
    let slider_h_pad = split[0].width.saturating_sub(slider_width) / 2;
    let slider_area = Rect {
        x: split[0].x + slider_h_pad,
        y: split[0].y,
        width: slider_width,
        height: split[0].height,
    };
    render_topup_slider(
        frame,
        slider_area,
        slider_title_spans(amount_pos),
        amount_pos,
        TOPUP_MAX_STEPS,
        &[(0, "any"), (5, "$5"), (10, "$10"), (25, "$25")],
    );

    // ── QR (or success checkmark) ───────────────────────────────────────
    if let Some(b) = blink {
        render_success_checkmark(frame, qr_rect, b.visible);
    } else {
        frame.render_widget(
            Paragraph::new(qr.lines).style(Style::default().bg(TOPUP_MAIN_BG).fg(Color::White)),
            qr_rect,
        );
    }

    // ── scan label + dimmed helper ──────────────────────────────────────
    let scan_label = Paragraph::new(Line::from(Span::styled(
        scan_banner_text(account_name, amount_pos),
        Style::default().fg(Color::White).bold().bg(TOPUP_MAIN_BG),
    )))
    .style(Style::default().bg(TOPUP_MAIN_BG))
    .centered();
    frame.render_widget(scan_label, split[2]);

    let helper = Paragraph::new(Line::from(Span::styled(
        "Requires Solana wallet mobile application",
        Style::default().fg(Color::DarkGray).bg(TOPUP_MAIN_BG),
    )))
    .style(Style::default().bg(TOPUP_MAIN_BG))
    .centered();
    frame.render_widget(helper, split[3]);

    let address = Paragraph::new(Line::from(Span::styled(
        pubkey.to_string(),
        Style::default().fg(Color::DarkGray).bg(TOPUP_MAIN_BG),
    )))
    .style(Style::default().bg(TOPUP_MAIN_BG))
    .centered();
    frame.render_widget(address, split[4]);
}

/// Build the slider title for the QR view — e.g.
/// ` Top-up amount: $3 (← → to adjust) `. Pure helper so we can unit-test
/// the wording without spinning up a Frame.
fn slider_title_spans<'a>(amount_pos: usize) -> Vec<Span<'a>> {
    let amount_str = format!(
        "{:>width$}",
        topup_amount_label(amount_pos),
        width = TOPUP_AMOUNT_LABEL_WIDTH
    );
    vec![
        Span::raw("Top-up amount: "),
        Span::styled(amount_str, Style::default().fg(Color::Green).bold()),
        Span::styled(" (← → to adjust)", Style::default().dim()),
    ]
}

fn topup_amount_label(amount_pos: usize) -> String {
    if amount_pos == 0 {
        "any".to_string()
    } else {
        format!("${:.0}", amount_pos as f64 * TOPUP_STEP_USDC)
    }
}

/// Borderless slider used in the QR view. Three rows: centered title,
/// the bar with arrows, and the scale labels. No surrounding box —
/// the caller positions and sizes `area` to control the visual width.
fn render_topup_slider<'a>(
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
        bar_spans.push(Span::styled(TOPUP_SLIDER_CELL, Style::default().fg(color)));
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

fn solana_pay_url(pubkey: &str, amount_pos: usize) -> String {
    if amount_pos > 0 {
        let amount = (amount_pos as f64) * TOPUP_STEP_USDC;
        format!("solana:{pubkey}?amount={amount}&spl-token={USDC_MINT}")
    } else {
        format!("solana:{pubkey}?spl-token={USDC_MINT}")
    }
}

#[allow(clippy::too_many_arguments)]
fn render_buy_stablecoins_detail(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    account_name: &str,
    status: &PollStatus,
    payment_method: OnrampPaymentMethod,
    onramp: Option<&OnrampSession>,
    onramp_notice: Option<&str>,
    onramp_error: Option<&str>,
) {
    // Same panel for both states (pre-launch + post-launch). The intro
    // function decides which action-copy variant to show based on whether
    // an onramp session is in flight.
    render_buy_stablecoins_intro(
        frame,
        area,
        pubkey,
        account_name,
        status,
        payment_method,
        onramp,
        onramp_notice,
        onramp_error,
    );
}

/// Width reserved for each payment-method button.
const PAYMENT_BUTTON_WIDTH: u16 = 18;
/// Horizontal gap between adjacent payment-method buttons.
const PAYMENT_BUTTON_GAP: u16 = 2;
/// Max width of the centered "What are stablecoins?" info panel. The panel
/// is also capped to the available area width.
const STABLECOIN_PANEL_MAX_WIDTH: u16 = 128;
/// Reassuring explainer aimed at non-crypto users. One entry per line so we
/// control exactly where breaks happen — the info panel renders as-is, no
/// auto-wrap, so nothing can smash sentences together at narrow widths.
const STABLECOIN_DESCRIPTION_LINES: &[&str] = &[
    "Stablecoins are digital dollars: each one equals $1 USD, fully backed and regulated.",
    "They're high-resolution money — divisible down to micro-cents ($0.000001), so APIs can charge precisely what they deliver.",
    "They move at internet speed: send and settle in seconds, anywhere in the world.",
];

/// Render the "Buy stablecoins" pane: title + explainer info panel, three
/// brand-coloured payment-method buttons, and a short action description.
/// Used both before launch (the "press Enter to open …" prompt) and after
/// launch (a spinner + "Waiting for {method} payment …" line) so the
/// layout never reflows when the user kicks off the onramp.
#[allow(clippy::too_many_arguments)]
fn render_buy_stablecoins_intro(
    frame: &mut ratatui::Frame,
    area: Rect,
    pubkey: &str,
    account_name: &str,
    status: &PollStatus,
    payment_method: OnrampPaymentMethod,
    onramp: Option<&OnrampSession>,
    onramp_notice: Option<&str>,
    onramp_error: Option<&str>,
) {
    // ── info panel: title in the border top-left, dynamic height ───────
    //
    // Measure the wrapped description first so the panel grows to fit
    // narrow terminals (text wraps to more lines) up to a fixed cap. The
    // overall layout height for the panel is then `content + 2 borders`.
    let panel_width = STABLECOIN_PANEL_MAX_WIDTH.min(area.width);
    let text_width = panel_width.saturating_sub(4); // 2 borders + 2 padding
    let description_lines: Vec<Line> = STABLECOIN_DESCRIPTION_LINES
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                *l,
                Style::default().fg(Color::Gray).bg(TOPUP_MAIN_BG),
            ))
        })
        .collect();
    let wrapped = wrapped_line_count(STABLECOIN_DESCRIPTION_LINES, text_width);
    // Panel total height = wrapped content lines + 2 borders. Floor at 3
    // (1 content line + borders); cap at 7 (5 content lines + borders) —
    // the panel grows as the terminal narrows, up to a fixed max.
    let panel_height = (wrapped + 2).clamp(3, 7);

    // Pre-launch: just a thin spacer between buttons and action copy.
    // Post-launch: replace it with a 4-row "money-flow" animation in the
    // selected brand's colour.
    let flow_height: u16 = if onramp.is_some() { 4 } else { 1 };

    let split = Layout::vertical([
        Constraint::Length(1),            // top spacer
        Constraint::Length(panel_height), // info panel (dynamic)
        Constraint::Length(1),            // spacer
        Constraint::Length(3),            // brand button(s)
        Constraint::Length(flow_height),  // flow animation OR spacer
        Constraint::Min(0),               // action copy
    ])
    .split(area);

    let info_h_pad = split[1].width.saturating_sub(panel_width) / 2;
    let info_area = Rect {
        x: split[1].x + info_h_pad,
        y: split[1].y,
        width: panel_width,
        height: split[1].height,
    };
    let info_block = Block::default()
        .title(Span::styled(
            " What are stablecoins? ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Center)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
        .style(Style::default().bg(TOPUP_MAIN_BG));
    let description = Paragraph::new(description_lines)
        .block(info_block)
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(ratatui::widgets::Wrap { trim: true });
    frame.render_widget(description, info_area);

    // Spinner phase, used both by the action-copy primary line and the
    // money-flow animation so they tick in lockstep.
    let spinner_tick = match status {
        PollStatus::Waiting { spinner_idx, .. }
        | PollStatus::Checking { spinner_idx }
        | PollStatus::Polling { spinner_idx, .. } => *spinner_idx,
        PollStatus::Stalled => 0,
    };

    // ── payment buttons + (when launched) money-flow animation ─────────
    if let Some(session) = onramp {
        // Single centered button for the active method.
        let btn_pad = split[3].width.saturating_sub(PAYMENT_BUTTON_WIDTH) / 2;
        let btn_area = Rect {
            x: split[3].x + btn_pad,
            y: split[3].y,
            width: PAYMENT_BUTTON_WIDTH,
            height: split[3].height,
        };
        render_payment_button(frame, btn_area, session.payment_method, true);
        render_money_flow(
            frame,
            split[4],
            session.payment_method.brand_color(),
            spinner_tick,
        );
    } else {
        let methods = OnrampPaymentMethod::all();
        let total_buttons_width =
            PAYMENT_BUTTON_WIDTH * methods.len() as u16 + PAYMENT_BUTTON_GAP * 2;
        let row_pad = split[3].width.saturating_sub(total_buttons_width) / 2;
        let mut button_constraints = vec![Constraint::Length(row_pad)];
        for i in 0..methods.len() {
            button_constraints.push(Constraint::Length(PAYMENT_BUTTON_WIDTH));
            if i + 1 < methods.len() {
                button_constraints.push(Constraint::Length(PAYMENT_BUTTON_GAP));
            }
        }
        button_constraints.push(Constraint::Min(0));
        let columns = Layout::horizontal(button_constraints).split(split[3]);

        for (idx, method) in methods.iter().enumerate() {
            // Skip the leading pad chunk (idx 0) and any gap chunks.
            let chunk_idx = 1 + idx * 2;
            let is_selected = *method == payment_method;
            render_payment_button(frame, columns[chunk_idx], *method, is_selected);
        }
    }

    // ── action copy ────────────────────────────────────────────────────
    // Closing copy ("Once funded …") + optional notice/error are shared
    // between pre- and post-launch states.
    let mut closing_lines = vec![
        Line::from(Span::styled(
            format!(
                "Once funded, your pay account — secured by {} — can spend on any pay-enabled API.",
                host_keystore_name()
            ),
            Style::default().fg(Color::DarkGray),
        ))
        .centered(),
    ];
    if let Some(notice) = onramp_notice {
        closing_lines.push(Line::raw(""));
        closing_lines.push(
            Line::from(Span::styled(
                notice.to_string(),
                Style::default().fg(Color::Yellow),
            ))
            .centered(),
        );
    }
    if let Some(err) = onramp_error {
        closing_lines.push(Line::raw(""));
        closing_lines.push(
            Line::from(Span::styled(
                format!("Last attempt failed: {err}"),
                Style::default().fg(Color::Red),
            ))
            .centered(),
        );
        closing_lines.push(
            Line::from(Span::styled(
                "Press Enter to retry.",
                Style::default().fg(Color::DarkGray),
            ))
            .centered(),
        );
    }

    if onramp.is_some() {
        // Post-launch: replace the spinner + Solana lines with a small
        // bordered destination box (account name + dimmed network/pubkey),
        // then the shared closing copy below it.
        let action_split = Layout::vertical([
            Constraint::Length(4), // destination box (2 borders + 2 content)
            Constraint::Length(1), // spacer
            Constraint::Min(0),    // closing copy
        ])
        .split(split[5]);

        const DEST_BOX_MAX_WIDTH: u16 = 52;
        let box_width = DEST_BOX_MAX_WIDTH.min(action_split[0].width);
        let box_pad = action_split[0].width.saturating_sub(box_width) / 2;
        let box_area = Rect {
            x: action_split[0].x + box_pad,
            y: action_split[0].y,
            width: box_width,
            height: action_split[0].height,
        };
        let dest_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::White))
            .padding(Padding::horizontal(1))
            .style(Style::default().bg(TOPUP_MAIN_BG));
        let dest_lines = vec![
            Line::from(Span::styled(
                account_name.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ))
            .centered(),
            Line::from(Span::styled(
                format!("[{pubkey}]"),
                Style::default().fg(Color::DarkGray),
            ))
            .centered(),
        ];
        let dest = Paragraph::new(dest_lines)
            .block(dest_block)
            .style(Style::default().bg(TOPUP_MAIN_BG));
        frame.render_widget(dest, box_area);

        let closing = Paragraph::new(closing_lines).style(Style::default().bg(TOPUP_MAIN_BG));
        frame.render_widget(closing, action_split[2]);
    } else {
        // Pre-launch: keep the original 3-line copy.
        let mut action_lines = vec![
            Line::from(vec![
                Span::styled("Press ", Style::default().fg(Color::Gray)),
                Span::styled(
                    "Enter",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " to open {} and convert dollars to USDC.",
                        payment_method.title()
                    ),
                    Style::default().fg(Color::Gray),
                ),
            ])
            .centered(),
            Line::from(vec![
                Span::styled(
                    "On Solana, funds arrive at ",
                    Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    pubkey.to_string(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" within a second.", Style::default().fg(Color::Gray)),
            ])
            .centered(),
        ];
        action_lines.extend(closing_lines);

        let action = Paragraph::new(action_lines).style(Style::default().bg(TOPUP_MAIN_BG));
        frame.render_widget(action, split[5]);
    }
}

/// Estimate the number of visual lines `text_lines` will occupy when each
/// is word-wrapped to `text_width` columns. Rough but reliable: counts
/// words and packs them greedily, matching the behaviour of
/// `Paragraph::wrap` closely enough to size a layout slot. Always returns
/// at least 1.
fn wrapped_line_count(text_lines: &[&str], text_width: u16) -> u16 {
    if text_width == 0 {
        return text_lines.len().max(1) as u16;
    }
    let width = text_width as usize;
    let mut total: u16 = 0;
    for line in text_lines {
        let mut used = 0usize;
        let mut lines_for_this = 1u16;
        for word in line.split_whitespace() {
            let w = word.chars().count();
            if used == 0 {
                used = w;
            } else if used + 1 + w <= width {
                used += 1 + w;
            } else {
                lines_for_this += 1;
                used = w;
            }
        }
        total = total.saturating_add(lines_for_this);
    }
    total.max(1)
}

/// Human-readable name for the OS keystore that backs the pay account.
/// Used in user-facing copy that reassures non-crypto users their account
/// is secured by something they already trust.
const fn host_keystore_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Apple Keychain"
    } else if cfg!(target_os = "windows") {
        "Credential Manager"
    } else {
        "your system keyring"
    }
}

/// 3-line button cell rendered like the option cards in the left pane:
/// brand-coloured background when selected, dark-gray otherwise. Title is
/// vertically centered, white bold.
fn render_payment_button(
    frame: &mut ratatui::Frame,
    area: Rect,
    method: OnrampPaymentMethod,
    is_selected: bool,
) {
    let bg = if is_selected {
        method.brand_color()
    } else {
        TOPUP_CARD_BG
    };
    // Selected text is white by default, but flip to black on light brand
    // backgrounds (Apple Pay) so the title stays legible.
    let fg = if is_selected {
        method.brand_text_color()
    } else {
        Color::Gray
    };
    let block = Block::default().style(Style::default().bg(bg));
    let card = Paragraph::new(vec![
        Line::default(),
        Line::from(Span::styled(
            method.title(),
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        ))
        .centered(),
    ])
    .block(block);
    frame.render_widget(card, area);
}

/// Vertical "money is flowing in" animation rendered beneath the active
/// payment-method button. Each row shows either a bright `▼`, a dim `▽`,
/// or a blank, with the bright glyph drifting downward as `tick`
/// advances — giving the impression of falling drops in the brand colour.
fn render_money_flow(frame: &mut ratatui::Frame, area: Rect, color: Color, tick: usize) {
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
    Ok(render_qr_code(&code, max_width, max_height))
}

fn render_topup_qr(
    data: &str,
    pubkey: &str,
    max_width: u16,
    max_height: u16,
) -> Result<Option<RenderedQr>, qrcode::types::QrError> {
    let version = topup_qr_version(pubkey)?;
    let stable = render_qr_with_version(data, version, max_width, max_height)?;

    if stable.is_some() {
        Ok(stable)
    } else {
        render_qr(data, max_width, max_height)
    }
}

fn topup_qr_version(pubkey: &str) -> Result<QrVersion, qrcode::types::QrError> {
    let sizing_url = solana_pay_url(pubkey, TOPUP_MAX_STEPS);
    let code = QrCode::with_error_correction_level(sizing_url.as_bytes(), qrcode::EcLevel::L)?;
    Ok(code.version())
}

fn render_qr_with_version(
    data: &str,
    version: QrVersion,
    max_width: u16,
    max_height: u16,
) -> Result<Option<RenderedQr>, qrcode::types::QrError> {
    let code = QrCode::with_version(data.as_bytes(), version, qrcode::EcLevel::L)?;
    Ok(render_qr_code(&code, max_width, max_height))
}

fn render_qr_code(code: &QrCode, max_width: u16, max_height: u16) -> Option<RenderedQr> {
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

    const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let status_spans = match status {
        PollStatus::Waiting { spinner_idx, .. }
        | PollStatus::Checking { spinner_idx }
        | PollStatus::Polling { spinner_idx, .. } => vec![
            Span::styled(
                SPINNER[spinner_idx % SPINNER.len()],
                Style::default().fg(Color::Green).bold(),
            ),
            Span::styled(
                " waiting for transfer…",
                Style::default().fg(Color::Green).bold(),
            ),
        ],
        PollStatus::Stalled => vec![
            Span::styled("stopped  ", Style::default().fg(Color::Yellow).bold()),
            Span::styled("R", Style::default().fg(Color::Cyan).bold()),
            Span::styled(" refresh", Style::default().fg(Color::Yellow).bold()),
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

fn print_topup_instructions(pubkey: &str, onramp_host: &str) {
    eprintln!("Top up your pay account:");
    eprintln!("  Address: {pubkey}");
    eprintln!("  1. Transfer funds from an existing Solana account.");
    let url = build_onramp_url(onramp_host, pubkey, None);
    eprintln!("  2. Buy funds with MoonPay: {url}");
}

fn build_onramp_redirect_url(host: &str) -> String {
    format!("{host}/v1/onramp/complete")
}

/// Compose the onramp URL we open in the browser. Points at the gateway's
/// `/v1/onramp/start` endpoint, which redirects to MoonPay with the server-side
/// API key + currency defaults applied.
fn build_onramp_url(
    host: &str,
    pubkey: &str,
    payment_method: Option<OnrampPaymentMethod>,
) -> String {
    let base = format!("{}/v1/onramp/start", host.trim_end_matches('/'));
    let redirect_url = build_onramp_redirect_url(host);
    let mut url = reqwest::Url::parse(&base).expect("onramp URL should parse");
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("walletAddress", pubkey);
        query.append_pair("redirectURL", &redirect_url);
        if let Some(payment_method) = payment_method {
            query.append_pair("paymentMethod", payment_method.query_value());
        }
    }
    url.into()
}

/// Launch a fresh onramp session in the browser.
fn launch_onramp_session(
    onramp_host: &str,
    pubkey: &str,
    payment_method: OnrampPaymentMethod,
) -> Result<OnrampSession, String> {
    let host = onramp_host.trim_end_matches('/').to_string();
    let url = build_onramp_url(&host, pubkey, Some(payment_method));
    open_url(&url).map_err(|err| format!("failed to open onramp: {err}"))?;
    Ok(OnrampSession {
        url,
        payment_method,
    })
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
                        u64::MAX
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
    let is_no_cap = position >= MAX_STEPS;
    let amount_str = if is_no_cap {
        "No cap".to_string()
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
            (31, "No cap"),
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

    let is_no_cap = budget_pos >= MAX_STEPS;
    let dollars = (budget_pos as f64) * 0.50;
    let budget_str = if is_no_cap {
        " No cap ".to_string()
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

    const SAMPLE_SOLANA_PAY_URL: &str = "solana:11111111111111111111111111111111?amount=5&spl-token=\
         EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    // ── PollState tests ─────────────────────────────────────────────────
    //
    // The state machine takes `now: Instant` on every time-dependent call
    // so we can advance "time" deterministically by adding Durations to a
    // fixed origin.

    fn healthy_baseline() -> AccountBalances {
        AccountBalances::default()
    }

    fn unhealthy_baseline() -> AccountBalances {
        AccountBalances {
            tokens_unavailable: true,
            ..Default::default()
        }
    }

    #[test]
    fn poll_state_filters_unhealthy_baseline_at_construction() {
        let now = Instant::now();
        let state = PollState::new(now, Some(unhealthy_baseline()));
        assert!(!state.has_healthy_baseline());
        assert!(state.baseline.is_none());
    }

    #[test]
    fn poll_state_idle_during_initial_delay() {
        let now = Instant::now();
        let state = PollState::new(now, Some(healthy_baseline()));
        assert_eq!(state.decide(now), PollDecision::Idle);
        assert_eq!(
            state.decide(now + POLL_DELAY - Duration::from_millis(1)),
            PollDecision::Idle
        );
    }

    #[test]
    fn poll_state_first_check_at_delay_boundary() {
        let now = Instant::now();
        let state = PollState::new(now, Some(healthy_baseline()));
        assert_eq!(state.decide(now + POLL_DELAY), PollDecision::SpawnCheck);
    }

    #[test]
    fn poll_state_spawns_even_without_baseline_to_establish_one() {
        // No baseline → check immediately so the thread can promote a
        // healthy fetch into the new baseline right after the first paint.
        let now = Instant::now();
        let state = PollState::new(now, None);
        assert_eq!(state.decide(now), PollDecision::SpawnCheck);
    }

    #[test]
    fn poll_state_does_not_double_spawn_while_checking() {
        let now = Instant::now();
        let mut state = PollState::new(now, Some(healthy_baseline()));
        let t = now + POLL_DELAY;
        state.on_check_started(t);
        assert_eq!(
            state.decide(t + Duration::from_millis(500)),
            PollDecision::Idle
        );
        assert_eq!(state.decide(t + POLL_INTERVAL), PollDecision::Idle);
    }

    #[test]
    fn poll_state_throttles_to_one_per_second() {
        let now = Instant::now();
        let mut state = PollState::new(now, Some(healthy_baseline()));
        let t0 = now + POLL_DELAY;
        state.on_check_started(t0);
        state.on_check_done();
        // Same second: no spawn.
        assert_eq!(
            state.decide(t0 + Duration::from_millis(500)),
            PollDecision::Idle
        );
        // POLL_INTERVAL elapsed: spawn.
        assert_eq!(state.decide(t0 + POLL_INTERVAL), PollDecision::SpawnCheck);
    }

    #[test]
    fn poll_state_stalls_after_window() {
        let now = Instant::now();
        let state = PollState::new(now, Some(healthy_baseline()));
        let after = now + POLL_WINDOW + Duration::from_secs(1);
        assert_eq!(state.decide(after), PollDecision::Idle);
        assert_eq!(state.status(after), PollStatus::Stalled);
    }

    #[test]
    fn poll_state_stalled_state_does_not_spawn_even_without_recent_check() {
        // Edge: never spawned, but window already elapsed (e.g. user idled
        // through the entire 5 minutes without UI interaction).
        let now = Instant::now();
        let state = PollState::new(now, Some(healthy_baseline()));
        assert_eq!(
            state.decide(now + POLL_WINDOW + Duration::from_secs(10)),
            PollDecision::Idle
        );
    }

    #[test]
    fn poll_state_reset_cycle_clears_stalled() {
        let now = Instant::now();
        let mut state = PollState::new(now, Some(healthy_baseline()));
        let stalled_at = now + POLL_WINDOW + Duration::from_secs(1);
        assert_eq!(state.status(stalled_at), PollStatus::Stalled);

        state.reset_cycle(stalled_at);
        // After reset the cycle restarts at `stalled_at` — back to Waiting.
        assert!(matches!(
            state.status(stalled_at),
            PollStatus::Waiting { .. }
        ));
        // And the first check fires again at +POLL_DELAY from the reset.
        assert_eq!(
            state.decide(stalled_at + POLL_DELAY),
            PollDecision::SpawnCheck
        );
    }

    #[test]
    fn poll_state_reset_cycle_clears_last_check_at() {
        let now = Instant::now();
        let mut state = PollState::new(now, Some(healthy_baseline()));
        state.on_check_started(now + POLL_DELAY);
        state.on_check_done();
        state.reset_cycle(now + POLL_DELAY);
        // After reset, decide() should treat us as fresh (Waiting), not
        // immediately ready for another check.
        assert_eq!(state.decide(now + POLL_DELAY), PollDecision::Idle);
    }

    #[test]
    fn poll_state_baseline_established_promotes_to_healthy() {
        let now = Instant::now();
        let mut state = PollState::new(now, None);
        assert!(!state.has_healthy_baseline());
        state.on_baseline_established(healthy_baseline());
        assert!(state.has_healthy_baseline());
        assert!(!state.checking);
    }

    #[test]
    fn poll_state_baseline_established_rejects_unhealthy_payload() {
        let now = Instant::now();
        let mut state = PollState::new(now, None);
        state.on_baseline_established(unhealthy_baseline());
        // Defensive: an unhealthy fetch must never become a baseline.
        assert!(!state.has_healthy_baseline());
        assert!(!state.checking);
    }

    #[test]
    fn poll_state_clear_stuck_check_releases_after_timeout() {
        let now = Instant::now();
        let mut state = PollState::new(now, Some(healthy_baseline()));
        let t = now + POLL_DELAY;
        state.on_check_started(t);
        // Just before timeout: still considered checking.
        state.clear_stuck_check(t + CHECK_TIMEOUT - Duration::from_millis(1));
        assert!(state.checking);
        // At/after timeout: cleared.
        state.clear_stuck_check(t + CHECK_TIMEOUT);
        assert!(!state.checking);
    }

    #[test]
    fn poll_state_status_waiting_counts_down() {
        let now = Instant::now();
        let state = PollState::new(now, Some(healthy_baseline()));
        match state.status(now) {
            PollStatus::Waiting { secs_left, .. } => {
                assert_eq!(secs_left, POLL_DELAY.as_secs());
            }
            other => panic!("expected Waiting, got {other:?}"),
        }
    }

    #[test]
    fn poll_state_status_polling_reports_window_remaining() {
        let now = Instant::now();
        let mut state = PollState::new(now, Some(healthy_baseline()));
        let t = now + POLL_DELAY + Duration::from_secs(10);
        state.on_check_started(t);
        state.on_check_done();
        match state.status(t) {
            PollStatus::Polling {
                secs_left_in_window,
                ..
            } => {
                let elapsed = (POLL_DELAY + Duration::from_secs(10)).as_secs();
                assert_eq!(secs_left_in_window, POLL_WINDOW.as_secs() - elapsed);
            }
            other => panic!("expected Polling, got {other:?}"),
        }
    }

    #[test]
    fn poll_state_status_checking_during_in_flight() {
        let now = Instant::now();
        let mut state = PollState::new(now, Some(healthy_baseline()));
        let t = now + POLL_DELAY;
        state.on_check_started(t);
        assert!(matches!(
            state.status(t + Duration::from_millis(50)),
            PollStatus::Checking { .. }
        ));
    }

    // ── diff_received resilience tests are over in pay-core ─────────────
    // (see crates/core/src/client/balance.rs `diff_received_*` tests).

    // ── scan_banner_text ───────────────────────────────────────────────

    #[test]
    fn scan_banner_text_uses_dollar_amount_for_nonzero_pos() {
        assert_eq!(
            scan_banner_text("default", 3),
            "Scan to send $3 USDC to @default"
        );
    }

    #[test]
    fn scan_banner_text_handles_any_amount() {
        assert_eq!(
            scan_banner_text("default", 0),
            "Scan to send any amount of USDC to @default"
        );
    }

    #[test]
    fn scan_banner_text_includes_account_name() {
        assert!(scan_banner_text("alice", 5).contains("@alice"));
    }

    #[test]
    fn topup_amount_label_formats_any_and_dollar_steps() {
        assert_eq!(topup_amount_label(0), "any");
        assert_eq!(topup_amount_label(3), "$3");
        assert_eq!(topup_amount_label(25), "$25");
    }

    #[test]
    fn topup_slider_title_keeps_amount_slot_stable() {
        for (amount_pos, expected) in [(0, "any"), (1, " $1"), (3, " $3"), (10, "$10"), (25, "$25")]
        {
            let spans = slider_title_spans(amount_pos);
            let amount = spans[1].content.as_ref();

            assert_eq!(amount, expected);
            assert_eq!(amount.chars().count(), TOPUP_AMOUNT_LABEL_WIDTH);
        }
    }

    #[test]
    fn topup_slider_cell_is_single_char_slot() {
        assert_eq!(TOPUP_SLIDER_CELL, "▐");
        assert_eq!(TOPUP_SLIDER_CELL.chars().count(), 1);
    }

    #[test]
    fn render_scale_spans_keeps_edge_labels_inside_track() {
        let spans = render_scale_spans(
            40,
            TOPUP_MAX_STEPS,
            33,
            &[(0, "any"), (TOPUP_MAX_STEPS, "$25")],
        );
        let line = spans[0].content.as_ref();

        assert_eq!(line.len(), 40);
        assert_eq!(&line[0..3], "   ");
        assert_eq!(&line[3..6], "any");
        assert_eq!(&line[34..37], "$25");
        assert_eq!(&line[37..40], "   ");
    }

    // ── wrapped_line_count ─────────────────────────────────────────────

    #[test]
    fn wrapped_line_count_short_lines_unchanged() {
        let lines = ["hello world", "foo bar"];
        assert_eq!(wrapped_line_count(&lines, 80), 2);
    }

    #[test]
    fn wrapped_line_count_wraps_when_width_is_narrow() {
        // "hello world" is 11 chars; at width 5 the words don't both fit.
        let lines = ["hello world"];
        assert_eq!(wrapped_line_count(&lines, 5), 2);
    }

    #[test]
    fn wrapped_line_count_zero_width_falls_back_to_line_count() {
        let lines = ["a", "b", "c"];
        assert_eq!(wrapped_line_count(&lines, 0), 3);
    }

    #[test]
    fn wrapped_line_count_empty_input_is_at_least_one() {
        assert_eq!(wrapped_line_count(&[], 80), 1);
    }

    #[test]
    fn wrapped_line_count_actual_description_at_full_width_is_three() {
        // At the panel's max inner width (128 - 4 = 124), the live
        // description's three logical lines each fit on one rendered line.
        assert_eq!(wrapped_line_count(STABLECOIN_DESCRIPTION_LINES, 124), 3);
    }

    #[test]
    fn wrapped_line_count_actual_description_at_narrow_width_grows() {
        // Halve the inner width — the longest sentence will need to wrap,
        // so total > 3 (we just want to confirm it grows past 3, not the
        // exact count which depends on word boundaries).
        let total = wrapped_line_count(STABLECOIN_DESCRIPTION_LINES, 60);
        assert!(total > 3, "expected growth past 3 lines, got {total}");
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
    fn topup_qr_render_keeps_dimensions_stable_across_amounts() {
        let pubkey = "11111111111111111111111111111111";
        let any = render_topup_qr(&solana_pay_url(pubkey, 0), pubkey, 120, 60)
            .expect("any QR should encode")
            .expect("any QR should fit");
        let max = render_topup_qr(&solana_pay_url(pubkey, TOPUP_MAX_STEPS), pubkey, 120, 60)
            .expect("max QR should encode")
            .expect("max QR should fit");

        assert_eq!(any.width, max.width);
        assert_eq!(any.height, max.height);
    }

    #[test]
    fn topup_qr_version_matches_max_amount_payload() {
        let pubkey = "11111111111111111111111111111111";
        let max_url = solana_pay_url(pubkey, TOPUP_MAX_STEPS);
        let max_code = QrCode::with_error_correction_level(max_url.as_bytes(), qrcode::EcLevel::L)
            .expect("max QR should encode");

        assert_eq!(topup_qr_version(pubkey).unwrap(), max_code.version());
    }

    #[test]
    fn topup_qr_render_refuses_to_clip() {
        let qr = render_qr(SAMPLE_SOLANA_PAY_URL, 1, 1).expect("QR should encode");

        assert!(qr.is_none());
    }

    #[test]
    fn build_onramp_redirect_url_targets_done_page() {
        assert_eq!(
            build_onramp_redirect_url("https://api.gateway-402.com"),
            "https://api.gateway-402.com/v1/onramp/complete"
        );
    }

    #[test]
    fn build_onramp_url_targets_gateway_with_fixed_params() {
        let url = build_onramp_url(
            "https://api.gateway-402.com",
            "wallet123",
            Some(OnrampPaymentMethod::Paypal),
        );
        let parsed = reqwest::Url::parse(&url).expect("onramp URL should parse");
        let query = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(parsed.host_str(), Some("api.gateway-402.com"));
        assert_eq!(parsed.path(), "/v1/onramp/start");
        assert_eq!(query.get("walletAddress"), Some(&"wallet123".into()));
        assert_eq!(
            query.get("redirectURL"),
            Some(&"https://api.gateway-402.com/v1/onramp/complete".into())
        );
        assert_eq!(query.get("paymentMethod"), Some(&"paypal".into()));
        assert!(!query.contains_key("apiKey"));
        assert!(!query.contains_key("currencyCode"));
        assert!(!query.contains_key("baseCurrencyAmount"));
        assert!(!query.contains_key("externalTransactionId"));
        assert!(!query.contains_key("account"));
    }

    #[test]
    fn build_onramp_url_omits_payment_method_when_absent() {
        let url = build_onramp_url("https://api.gateway-402.com", "wallet123", None);
        let parsed = reqwest::Url::parse(&url).expect("onramp URL should parse");
        let query = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(parsed.path(), "/v1/onramp/start");
        assert!(!query.contains_key("paymentMethod"));
    }

    #[test]
    fn default_onramp_host_matches_pay_api_host() {
        // Both the balance API and the onramp endpoint live on the same
        // gateway — keep them locked to a single source of truth.
        assert_eq!(
            DEFAULT_ONRAMP_HOST,
            pay_core::client::balance::DEFAULT_PAY_API_URL
        );
    }
}
