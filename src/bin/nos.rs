use std::fs::OpenOptions;
use std::io;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use engine_rs::birdeye::{BirdeyeClient, QuoteCurrency};
use engine_rs::exec::Executor;
use engine_rs::helpers::read_keypair_from_file;
use engine_rs::info::StreamManager;
use engine_rs::{
    BalanceSync, EngineCommand, EngineView, ExecParam, ExecParams, IndicatorKind, MarketCommand,
    OpenPosInfo, Price, SignalEngine, Strategy, TimeFrame,
};
use log::{info, warn};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use solana_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::Signer;
use tokio_stream::StreamExt;

use engine_rs::Value;

const MAINNET_RPC: &str = "https://api.mainnet-beta.solana.com";
const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const SLIPPAGE_BPS: u16 = 400;
const TUI_TICK_MS: u64 = 250;
const RECONNECT_DELAY_SECS: u64 = 2;
const BALANCE_REFRESH_SECS: u64 = 5;

enum AppEvent {
    Price(Price),
    Market(MarketCommand),
    Balance(u64),
}

#[derive(Debug)]
struct AppState {
    price: f64,
    rsi: Option<f64>,
    engine_state: EngineView,
    open_pos: Option<OpenPosInfo>,
    last_price_ts: Option<u64>,
    mint: String,
    total_pnl: f64,
    sol_balance_lamports: u64,
    wallet_pubkey: String,
}

impl AppState {
    fn upnl(&self) -> Option<f64> {
        let pos = self.open_pos?;
        if self.price <= 0.0 {
            return None;
        }
        Some((self.price - pos.entry_px) * pos.size)
    }

    fn in_trade(&self) -> bool {
        self.open_pos.is_some()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            price: 0.0,
            rsi: None,
            engine_state: EngineView::Idle,
            open_pos: None,
            last_price_ts: None,
            mint: String::new(),
            total_pnl: 0.0,
            sol_balance_lamports: 0,
            wallet_pubkey: String::new(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    init_logger();

    let birdeye_key = std::env::var("BIRDEYE_API_KEY").unwrap();
    let client = BirdeyeClient::new(birdeye_key);

    let keypair =
        read_keypair_from_file("./keypair.json").map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let wallet_pubkey = keypair.pubkey();
    let wallet_pubkey_str = wallet_pubkey.to_string();
    let token_str = std::env::var("TOKEN_MINT")
        .unwrap_or_else(|_| "GbqbsRyBHVPHEv7xLEsikXmdpPPiApLTJesDBbL5pump".to_string());

    let candles = client
        .fetch_candles(&token_str, TimeFrame::Min1, QuoteCurrency::Sol, 1000)
        .await?;

    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| MAINNET_RPC.to_string());
    let rpc_client = Arc::new(RpcClient::new(rpc_url));

    let (engine_tx, engine_rx) = tokio::sync::mpsc::unbounded_channel();
    let (trade_tx, trade_rx) = flume::bounded(0);
    let (market_tx, mut market_rx) = tokio::sync::mpsc::channel(256);
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();

    let indicators = vec![(IndicatorKind::Rsi(12), TimeFrame::Min1)];
    let balance = BalanceSync::new(Arc::clone(&rpc_client), wallet_pubkey).fetch_balance()?;
    let exec_params = ExecParams::new(balance);

    let mut engine = SignalEngine::new(
        Some(indicators),
        Strategy::RsiClassic,
        engine_rx,
        Some(market_tx.clone()),
        trade_tx,
        exec_params,
    )
    .await;
    engine.load(TimeFrame::Min1, candles.clone()).await;

    let sol_mint = Pubkey::from_str(SOL_MINT)?;
    let token_mint = Pubkey::from_str(&token_str)?;

    let wallet = Arc::new(keypair);
    let executor = Executor::new(
        trade_rx,
        Some(market_tx.clone()),
        Arc::clone(&rpc_client),
        wallet,
        sol_mint,
        token_mint,
        SLIPPAGE_BPS,
    )?;

    tokio::spawn(async move {
        executor.run().await;
    });

    tokio::spawn(async move {
        engine.start().await;
    });

    let engine_tx_market = engine_tx.clone();
    let event_tx_market = event_tx.clone();
    tokio::spawn(async move {
        while let Some(cmd) = market_rx.recv().await {
            match &cmd {
                MarketCommand::OpenPosition(pos) => {
                    let _ = engine_tx_market.send(EngineCommand::UpdateExecParams(
                        ExecParam::OpenPosition(*pos),
                    ));
                }
                MarketCommand::OpenFailed(reason) => {
                    let _ = engine_tx_market.send(EngineCommand::OpenFailed(reason.clone()));
                }
                _ => {}
            }
            let _ = event_tx_market.send(AppEvent::Market(cmd));
        }
    });

    let engine_tx_price = engine_tx.clone();
    let event_tx_price = event_tx.clone();
    tokio::spawn(async move {
        let mut manager = match StreamManager::new() {
            Ok(manager) => manager,
            Err(err) => {
                warn!("stream manager init failed: {}", err);
                return;
            }
        };

        loop {
            let (sub_id, mut rx) = match manager.subscribe_candles(token_mint).await {
                Ok(sub) => sub,
                Err(err) => {
                    warn!("candle subscribe failed: {}", err);
                    tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
                    continue;
                }
            };

            info!("candle stream subscribed: {}", sub_id);

            while let Some(candle) = rx.recv().await {
                let _ = engine_tx_price.send(EngineCommand::UpdatePrice(candle));
                let _ = event_tx_price.send(AppEvent::Price(candle));
            }

            warn!("candle stream ended; resubscribing");
            let _ = manager.unsubscribe_candles(sub_id);
            tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
        }
    });

    let event_tx_balance = event_tx.clone();
    let rpc_client_balance = Arc::clone(&rpc_client);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(BALANCE_REFRESH_SECS));
        loop {
            tick.tick().await;
            let balance = rpc_client_balance.get_balance(&wallet_pubkey).unwrap_or(0);
            let _ = event_tx_balance.send(AppEvent::Balance(balance));
        }
    });

    let mut terminal = setup_terminal()?;
    let result = run_tui(&mut terminal, &mut event_rx, token_str, wallet_pubkey_str).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    mint: String,
    wallet_pubkey: String,
) -> Result<()> {
    let mut app = AppState {
        engine_state: EngineView::Idle,
        mint,
        wallet_pubkey,
        ..Default::default()
    };
    let mut tick = tokio::time::interval(Duration::from_millis(TUI_TICK_MS));
    let mut input = EventStream::new();

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        tokio::select! {
            _ = tick.tick() => {}
            maybe_event = input.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                            _ => {}
                        }
                    }
                    Some(Err(err)) => {
                        warn!("input error: {}", err);
                    }
                    _ => {}
                }
            }
            maybe_msg = event_rx.recv() => {
                match maybe_msg {
                    Some(msg) => apply_event(&mut app, msg),
                    None => break,
                }
            }
        }
    }

    Ok(())
}

fn apply_event(app: &mut AppState, msg: AppEvent) {
    match msg {
        AppEvent::Price(price) => {
            app.price = price.close;
            app.last_price_ts = Some(price.close_time);
        }
        AppEvent::Market(cmd) => match cmd {
            MarketCommand::IndicatorData(indicators) => apply_indicators(app, indicators),
            MarketCommand::EngineState(state) => app.engine_state = state,
            MarketCommand::OpenPosition(pos) => app.open_pos = pos,
            MarketCommand::OpenFailed(_) => {
                app.open_pos = None;
                app.engine_state = EngineView::Idle;
            }
            MarketCommand::ExecutorPaused(_) => {}
            MarketCommand::SwapFill(_) => {}
            MarketCommand::Trade(trade) => {
                app.total_pnl += trade.pnl;
            }
        },
        AppEvent::Balance(balance) => app.sol_balance_lamports = balance,
    }
}

fn apply_indicators(app: &mut AppState, indicators: Vec<engine_rs::IndicatorData>) {
    for indicator in indicators {
        if let (IndicatorKind::Rsi(_), Some(Value::RsiValue(value))) =
            (indicator.id.0, indicator.value)
        {
            app.rsi = Some(value);
        }
    }
}

fn render(frame: &mut Frame, app: &AppState) {
    let label_width = 12;
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let header_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(layout[0]);

    frame.render_widget(Clear, header_layout[0]);
    let header_left = Paragraph::new(Line::from(vec![
        Span::styled(
            "NOS",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw("state: "),
        Span::styled(
            format_engine_state(app.engine_state),
            Style::default().fg(Color::Yellow),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(header_left, header_layout[0]);

    frame.render_widget(Clear, header_layout[1]);
    let sol_balance = format_sol(app.sol_balance_lamports);
    let wallet_line = format_wallet_line(app.wallet_pubkey.as_str(), header_layout[1].width);
    let header_right = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("SOL: ", Style::default().fg(Color::Gray)),
            Span::styled(sol_balance, Style::default().fg(Color::Green)),
        ]),
        Line::from(vec![
            Span::styled("Wallet: ", Style::default().fg(Color::Gray)),
            Span::raw(wallet_line),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header_right, header_layout[1]);

    let upnl = app.upnl();
    let upnl_span = match upnl {
        Some(value) => {
            let color = if value >= 0.0 {
                Color::Green
            } else {
                Color::Red
            };
            Span::styled(format!("{:.6}", value), Style::default().fg(color))
        }
        None => Span::raw("--"),
    };

    let in_trade = if app.in_trade() { "YES" } else { "NO" };
    let open_time = app
        .open_pos
        .map(|pos| format_time_ms(pos.open_time))
        .unwrap_or_else(|| "--".to_string());
    let entry_px = app
        .open_pos
        .map(|pos| format!("{:.8}", pos.entry_px))
        .unwrap_or_else(|| "--".to_string());
    let size = app
        .open_pos
        .map(|pos| format!("{:.8}", pos.size))
        .unwrap_or_else(|| "--".to_string());

    let body_lines = vec![
        kv_line(
            "Price:",
            Span::styled(format_price(app.price), Style::default().fg(Color::White)),
            label_width,
        ),
        kv_line(
            "RSI(12):",
            Span::styled(format_opt(app.rsi), Style::default().fg(Color::Magenta)),
            label_width,
        ),
        kv_line("In trade:", Span::raw(in_trade), label_width),
        kv_line("Size:", Span::raw(size), label_width),
        kv_line("Entry:", Span::raw(entry_px), label_width),
        kv_line("UPNL:", upnl_span, label_width),
        kv_line(
            "Total PnL:",
            Span::styled(
                format!("{:.6}", app.total_pnl),
                Style::default().fg(if app.total_pnl >= 0.0 {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
            label_width,
        ),
        kv_line("Open time:", Span::raw(open_time), label_width),
        kv_line(
            "Last price:",
            Span::raw(
                app.last_price_ts
                    .map(format_time_ms)
                    .unwrap_or_else(|| "--".to_string()),
            ),
            label_width,
        ),
        kv_line("CA:", Span::raw(app.mint.as_str()), label_width),
    ];

    frame.render_widget(Clear, layout[1]);
    let body =
        Paragraph::new(body_lines).block(Block::default().borders(Borders::ALL).title("Market"));
    frame.render_widget(body, layout[1]);

    frame.render_widget(Clear, layout[2]);
    let footer =
        Paragraph::new(Line::from("q to quit")).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[2]);
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn init_logger() {
    let log_path = std::env::var("NOS_LOG").unwrap_or_else(|_| "/tmp/nos.log".to_string());
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));

    if let Ok(file) = OpenOptions::new().create(true).append(true).open(log_path) {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }

    let _ = builder.try_init();
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn format_price(price: f64) -> String {
    if price > 0.0 {
        format!("{:.8}", price)
    } else {
        "--".to_string()
    }
}

fn format_opt(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{:.2}", v),
        None => "--".to_string(),
    }
}

fn format_time_ms(ts_ms: u64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts_ms as i64)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "--".to_string())
}

fn format_engine_state(state: EngineView) -> &'static str {
    match state {
        EngineView::Idle => "Idle",
        EngineView::Armed => "Armed",
        EngineView::Opening => "Opening",
        EngineView::Closing => "Closing",
        EngineView::Open => "Open",
    }
}

fn kv_line<'a>(label: &'a str, value: Span<'a>, width: usize) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{label:<width$}", label = label, width = width),
            Style::default().fg(Color::Gray),
        ),
        value,
    ])
}

fn format_sol(lamports: u64) -> String {
    let sol = lamports as f64 / LAMPORTS_PER_SOL as f64;
    format!("{:.6}", sol)
}

fn format_wallet_line(pubkey: &str, area_width: u16) -> String {
    let inner_width = area_width.saturating_sub(2) as usize;
    let label_width = "Wallet: ".len();
    let max_value = inner_width.saturating_sub(label_width);
    truncate_middle(pubkey, max_value)
}

fn truncate_middle(value: &str, max_len: usize) -> String {
    if max_len == 0 || value.is_empty() {
        return String::new();
    }
    if value.len() <= max_len {
        return value.to_string();
    }
    if max_len <= 3 {
        return ".".repeat(max_len);
    }
    let keep = (max_len - 3) / 2;
    let tail_keep = max_len - 3 - keep;
    let head = &value[..keep];
    let tail = &value[value.len().saturating_sub(tail_keep)..];
    format!("{head}...{tail}")
}
