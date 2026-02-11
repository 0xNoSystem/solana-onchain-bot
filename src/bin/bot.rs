use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use engine_rs::birdeye::BirdeyeClient;
use engine_rs::helpers::read_keypair_from_file;
use engine_rs::{
    AddMarketInfo, Bot, BotCommand, BotSnapshot, EngineView, MarginAllocation, MarketConfig,
    MarketState, Strategy, TimeFrame,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use solana_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::Signer;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;

const MAINNET_RPC: &str = "https://api.mainnet-beta.solana.com";
const UI_TICK_MS: u64 = 120;
const SNAPSHOT_REFRESH_MS: u64 = 500;
const ADD_RESPONSE_TIMEOUT_SECS: u64 = 15;
const DEFAULT_MARGIN_SOL: &str = "0.1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddField {
    Mint,
    Alloc,
    Strategy,
}

#[derive(Debug, Clone)]
struct AddForm {
    mint: String,
    margin_sol: String,
    strategy_idx: usize,
    active: AddField,
}

#[derive(Debug, Clone, Copy)]
struct StrategyOption {
    label: &'static str,
    strategy: Strategy,
}

const STRATEGY_OPTIONS: &[StrategyOption] = &[
    StrategyOption {
        label: "RsiClassic",
        strategy: Strategy::RsiClassic,
    },
    StrategyOption {
        label: "RsiEmaScalp",
        strategy: Strategy::RsiEmaScalp,
    },
    StrategyOption {
        label: "RsiChopSwing",
        strategy: Strategy::RsiChopSwing,
    },
    StrategyOption {
        label: "SrsiAdxScalp",
        strategy: Strategy::SrsiAdxScalp,
    },
    StrategyOption {
        label: "ElderTripleScreen",
        strategy: Strategy::ElderTripleScreen,
    },
    StrategyOption {
        label: "SmaRsiScalpTest",
        strategy: Strategy::SmaRsiScalpTest,
    },
];

impl Default for AddForm {
    fn default() -> Self {
        Self {
            mint: String::new(),
            margin_sol: DEFAULT_MARGIN_SOL.to_string(),
            strategy_idx: default_strategy_index(),
            active: AddField::Mint,
        }
    }
}

fn default_strategy_index() -> usize {
    STRATEGY_OPTIONS
        .iter()
        .position(|opt| opt.strategy == Strategy::RsiClassic)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
enum Mode {
    Normal,
    Add(AddForm),
}

#[derive(Debug, Clone)]
struct AppState {
    wallet_pubkey: String,
    snapshot: BotSnapshot,
    selected: usize,
    mode: Mode,
    status: String,
    should_quit: bool,
}

impl AppState {
    fn new(wallet_pubkey: String) -> Self {
        Self {
            wallet_pubkey,
            snapshot: BotSnapshot::default(),
            selected: 0,
            mode: Mode::Normal,
            status: "a add | d remove | p pause | up/down move | q quit".to_string(),
            should_quit: false,
        }
    }

    fn selected_market(&self) -> Option<&MarketState> {
        if self.snapshot.markets.is_empty() {
            None
        } else {
            self.snapshot.markets.get(self.selected)
        }
    }

    fn set_snapshot(&mut self, snapshot: BotSnapshot) {
        let selected_mint = self.selected_market().map(|m| m.token_mint);
        self.snapshot = snapshot;

        if self.snapshot.markets.is_empty() {
            self.selected = 0;
            return;
        }

        if let Some(mint) = selected_mint {
            if let Some(index) = self
                .snapshot
                .markets
                .iter()
                .position(|market| market.token_mint == mint)
            {
                self.selected = index;
                return;
            }
        }

        self.selected = self
            .selected
            .min(self.snapshot.markets.len().saturating_sub(1));
    }

    fn move_selection_up(&mut self) {
        if self.snapshot.markets.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.snapshot.markets.len().saturating_sub(1);
        } else {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    fn move_selection_down(&mut self) {
        if self.snapshot.markets.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.snapshot.markets.len();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    init_logger();

    let birdeye_key = std::env::var("BIRDEYE_API_KEY")
        .map_err(|_| anyhow!("missing BIRDEYE_API_KEY in environment"))?;
    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| MAINNET_RPC.to_string());
    let keypair_path =
        std::env::var("KEYPAIR_PATH").unwrap_or_else(|_| "./keypair.json".to_string());

    let keypair = read_keypair_from_file(&keypair_path)
        .map_err(|err| anyhow!("failed to read keypair {}: {}", keypair_path, err))?;
    let wallet_pubkey = keypair.pubkey().to_string();

    let rpc_client = Arc::new(RpcClient::new(rpc_url));
    let birdeye = BirdeyeClient::new(birdeye_key);
    let (bot, bot_tx) = Bot::new(rpc_client, Arc::new(keypair), birdeye);

    let bot_task = tokio::spawn(async move {
        if let Err(err) = bot.start().await {
            log::error!("bot task exited with error: {}", err);
        }
    });

    let mut terminal = setup_terminal()?;
    let run_res = run_tui(&mut terminal, bot_tx.clone(), wallet_pubkey).await;
    let restore_res = restore_terminal(&mut terminal);

    let _ = bot_tx.send(BotCommand::Kill).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), bot_task).await;

    restore_res?;
    run_res
}

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    bot_tx: Sender<BotCommand>,
    wallet_pubkey: String,
) -> Result<()> {
    let mut app = AppState::new(wallet_pubkey);
    refresh_snapshot(&mut app, &bot_tx).await;

    let mut ui_tick = tokio::time::interval(Duration::from_millis(UI_TICK_MS));
    let mut snapshot_tick = tokio::time::interval(Duration::from_millis(SNAPSHOT_REFRESH_MS));
    let mut input = EventStream::new();

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        tokio::select! {
            _ = ui_tick.tick() => {}
            _ = snapshot_tick.tick() => {
                refresh_snapshot(&mut app, &bot_tx).await;
            }
            _ = tokio::signal::ctrl_c() => {
                app.should_quit = true;
            }
            maybe_event = input.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key.code, key.modifiers, &bot_tx).await;
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        handle_mouse_event(mouse, &mut app);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        app.status = format!("input error: {}", err);
                    }
                    None => {
                        app.should_quit = true;
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

async fn refresh_snapshot(app: &mut AppState, bot_tx: &Sender<BotCommand>) {
    match fetch_snapshot(bot_tx).await {
        Ok(mut snapshot) => {
            snapshot
                .markets
                .sort_by_key(|market| market.token_mint.to_string());
            app.set_snapshot(snapshot)
        }
        Err(err) => app.status = format!("snapshot failed: {}", err),
    }
}

async fn fetch_snapshot(bot_tx: &Sender<BotCommand>) -> Result<BotSnapshot> {
    let (reply_tx, reply_rx) = oneshot::channel();
    tokio::time::timeout(
        Duration::from_millis(250),
        bot_tx.send(BotCommand::GetSnapshot(reply_tx)),
    )
    .await
    .map_err(|_| anyhow!("snapshot request timed out"))?
    .map_err(|err| anyhow!("bot command channel closed: {}", err))?;

    let snapshot = tokio::time::timeout(Duration::from_millis(450), reply_rx)
        .await
        .map_err(|_| anyhow!("snapshot response timed out"))?
        .map_err(|_| anyhow!("snapshot response dropped"))?;
    Ok(snapshot)
}

async fn handle_key(
    app: &mut AppState,
    code: KeyCode,
    modifiers: KeyModifiers,
    bot_tx: &Sender<BotCommand>,
) {
    match app.mode.clone() {
        Mode::Normal => handle_normal_key(app, code, modifiers, bot_tx).await,
        Mode::Add(_) => handle_add_key(app, code, modifiers, bot_tx).await,
    }
}

async fn handle_normal_key(
    app: &mut AppState,
    code: KeyCode,
    modifiers: KeyModifiers,
    bot_tx: &Sender<BotCommand>,
) {
    match code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.should_quit = true,
        KeyCode::Up | KeyCode::Char('k') => app.move_selection_up(),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection_down(),
        KeyCode::Char('a') => {
            app.mode = Mode::Add(AddForm::default());
            app.status =
                "add mode: mint, margin SOL, strategy | tab switch | <-/-> strategy | enter submit"
                    .to_string();
        }
        KeyCode::Char('d') => {
            if let Some(market) = app.selected_market() {
                let mint = market.token_mint;
                match bot_tx.send(BotCommand::RemoveMarket(mint)).await {
                    Ok(_) => {
                        app.status = format!("remove requested for {}", mint);
                    }
                    Err(err) => {
                        app.status = format!("remove failed to send: {}", err);
                    }
                }
            } else {
                app.status = "no market selected".to_string();
            }
        }
        KeyCode::Char('p') => {
            if let Some(market) = app.selected_market() {
                let mint = market.token_mint;
                match bot_tx.send(BotCommand::PauseMarket(mint)).await {
                    Ok(_) => {
                        app.status = format!("pause requested for {}", mint);
                    }
                    Err(err) => {
                        app.status = format!("pause failed to send: {}", err);
                    }
                }
            } else {
                app.status = "no market selected".to_string();
            }
        }
        _ => {}
    }
}

fn handle_mouse_event(mouse: MouseEvent, app: &mut AppState) {
    if !matches!(app.mode, Mode::Normal) {
        return;
    }
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }

    let (cols, rows) = match crossterm::terminal::size() {
        Ok(size) => size,
        Err(err) => {
            app.status = format!("mouse copy failed: {}", err);
            return;
        }
    };
    let size = Rect::new(0, 0, cols, rows);

    if wallet_click_hit(size, mouse.column, mouse.row) {
        match copy_to_clipboard_osc52(&app.wallet_pubkey) {
            Ok(()) => app.status = "copied wallet address".to_string(),
            Err(err) => app.status = format!("copy wallet failed: {}", err),
        }
        return;
    }

    if selected_ca_click_hit(size, mouse.column, mouse.row) {
        if let Some(ca) = app.selected_market().map(|m| m.token_mint.to_string()) {
            match copy_to_clipboard_osc52(&ca) {
                Ok(()) => app.status = format!("copied CA {}", ca),
                Err(err) => app.status = format!("copy CA failed: {}", err),
            }
        }
    }
}

fn wallet_click_hit(size: Rect, column: u16, row: u16) -> bool {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(size);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(root[0]);
    let wallet = top[1];

    if wallet.width <= 2 || wallet.height <= 3 {
        return false;
    }

    let content_x = wallet.x + 1;
    let content_y = wallet.y + 1;
    let value_row = content_y + 2; // third line in wallet header
    let value_start = content_x + "Wallet: ".len() as u16;
    let value_end = wallet.x + wallet.width - 1;

    row == value_row && column >= value_start && column < value_end
}

fn selected_ca_click_hit(size: Rect, column: u16, row: u16) -> bool {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(size);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(root[1]);
    let selected = body[1];

    if selected.width <= 2 || selected.height <= 2 {
        return false;
    }

    let content_x = selected.x + 1;
    let content_y = selected.y + 1;
    let value_row = content_y + 1; // second line is CA
    let value_start = content_x + 11; // kv_line label width
    let value_end = selected.x + selected.width - 1;

    row == value_row && column >= value_start && column < value_end
}

async fn handle_add_key(
    app: &mut AppState,
    code: KeyCode,
    modifiers: KeyModifiers,
    bot_tx: &Sender<BotCommand>,
) {
    let mut form = match app.mode.clone() {
        Mode::Add(form) => form,
        Mode::Normal => return,
    };

    match code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.status = "add cancelled".to_string();
            return;
        }
        KeyCode::Tab => {
            form.active = match form.active {
                AddField::Mint => AddField::Alloc,
                AddField::Alloc => AddField::Strategy,
                AddField::Strategy => AddField::Mint,
            };
        }
        KeyCode::Down => {
            form.active = match form.active {
                AddField::Mint => AddField::Alloc,
                AddField::Alloc => AddField::Strategy,
                AddField::Strategy => AddField::Mint,
            };
        }
        KeyCode::Up => {
            form.active = match form.active {
                AddField::Mint => AddField::Strategy,
                AddField::Alloc => AddField::Mint,
                AddField::Strategy => AddField::Alloc,
            };
        }
        KeyCode::Backspace => match form.active {
            AddField::Mint => {
                form.mint.pop();
            }
            AddField::Alloc => {
                form.margin_sol.pop();
            }
            AddField::Strategy => {}
        },
        KeyCode::Enter => match form.active {
            AddField::Mint => form.active = AddField::Alloc,
            AddField::Alloc => form.active = AddField::Strategy,
            AddField::Strategy => {
                submit_add(app, form, bot_tx).await;
                return;
            }
        },
        KeyCode::Left | KeyCode::Char('h') => {
            if form.active == AddField::Strategy {
                if form.strategy_idx == 0 {
                    form.strategy_idx = STRATEGY_OPTIONS.len().saturating_sub(1);
                } else {
                    form.strategy_idx = form.strategy_idx.saturating_sub(1);
                }
            }
        }
        KeyCode::Right | KeyCode::Char('l') => {
            if form.active == AddField::Strategy {
                form.strategy_idx = (form.strategy_idx + 1) % STRATEGY_OPTIONS.len();
            }
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
            return;
        }
        KeyCode::Char(ch) => match form.active {
            AddField::Mint => {
                if !ch.is_whitespace() {
                    form.mint.push(ch);
                }
            }
            AddField::Alloc => {
                if ch.is_ascii_digit() || ch == '.' {
                    form.margin_sol.push(ch);
                }
            }
            AddField::Strategy => {}
        },
        _ => {}
    }

    app.mode = Mode::Add(form);
}

async fn submit_add(app: &mut AppState, form: AddForm, bot_tx: &Sender<BotCommand>) {
    let mint_str = form.mint.trim().trim_matches('"').trim_matches('\'');
    if mint_str.is_empty() {
        app.status = "mint cannot be empty".to_string();
        app.mode = Mode::Add(form);
        return;
    }

    let mint = match Pubkey::from_str(mint_str) {
        Ok(mint) => mint,
        Err(err) => {
            app.status = format!("invalid mint: {}", err);
            app.mode = Mode::Add(form);
            return;
        }
    };

    let margin_sol = match form.margin_sol.trim().parse::<f64>() {
        Ok(value) if value.is_finite() && value > 0.0 => value,
        _ => {
            app.status = "margin SOL must be > 0.0".to_string();
            app.mode = Mode::Add(form);
            return;
        }
    };

    let strategy_opt = STRATEGY_OPTIONS
        .get(form.strategy_idx)
        .copied()
        .unwrap_or(STRATEGY_OPTIONS[default_strategy_index()]);

    let mut market_cfg = MarketConfig::new(mint);
    market_cfg.strategy = strategy_opt.strategy;
    market_cfg.timeframe = TimeFrame::Min1;
    market_cfg.indicators = strategy_opt.strategy.indicators();

    let add = AddMarketInfo {
        market_cfg,
        margin_alloc: MarginAllocation::AmountSol(margin_sol),
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    match bot_tx
        .send(BotCommand::AddMarket {
            info: add,
            reply: reply_tx,
        })
        .await
    {
        Ok(_) => {
            match tokio::time::timeout(Duration::from_secs(ADD_RESPONSE_TIMEOUT_SECS), reply_rx)
                .await
            {
                Ok(Ok(Ok(()))) => {
                    app.status = format!(
                        "add confirmed for {} ({:.4} SOL) strat={}",
                        mint, margin_sol, strategy_opt.label
                    );
                    app.mode = Mode::Normal;
                }
                Ok(Ok(Err(err))) => {
                    app.status = format!("add market rejected: {}", err);
                    app.mode = Mode::Add(form);
                }
                Ok(Err(_)) => {
                    app.status = format!(
                        "add request sent for {} ({:.4} SOL); still processing (response dropped)",
                        mint, margin_sol
                    );
                    app.mode = Mode::Normal;
                }
                Err(_) => {
                    app.status = format!(
                        "add request sent for {} ({:.4} SOL); still processing",
                        mint, margin_sol
                    );
                    app.mode = Mode::Normal;
                }
            }
        }
        Err(err) => {
            app.status = format!("failed to send add command: {}", err);
            app.mode = Mode::Add(form);
        }
    }
}

fn render(frame: &mut Frame, app: &AppState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(root[0]);
    render_status_header(frame, app, top[0]);
    render_wallet_header(frame, app, top[1]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(root[1]);
    render_market_list(frame, app, body[0]);
    render_selected_market(frame, app, body[1]);

    render_footer(frame, app, root[2]);

    if let Mode::Add(form) = &app.mode {
        render_add_modal(frame, form);
    }
}

fn render_status_header(frame: &mut Frame, app: &AppState, area: Rect) {
    let mode_label = match app.mode {
        Mode::Normal => "Normal",
        Mode::Add(_) => "Add Market",
    };
    let sol_price = format_sol_usd(app.snapshot.sol_price_usd);
    let sol_style = if app.snapshot.sol_price_usd.is_some() {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "BOT",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Mode: ", Style::default().fg(Color::Gray)),
            Span::styled(mode_label, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("Markets: ", Style::default().fg(Color::Gray)),
            Span::styled(
                app.snapshot.markets.len().to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("SOL/USD: ", Style::default().fg(Color::Gray)),
            Span::styled(sol_price, sol_style),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(header, area);
}

fn render_wallet_header(frame: &mut Frame, app: &AppState, area: Rect) {
    let total_sol = format_sol(app.snapshot.total_on_chain_lamports);
    let used_sol = format_sol(app.snapshot.used_lamports);
    let free_sol = format_sol(app.snapshot.free_lamports);

    let wallet = truncate_middle(
        app.wallet_pubkey.as_str(),
        area.width.saturating_sub(10) as usize,
    );
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Total: ", Style::default().fg(Color::Gray)),
            Span::styled(total_sol, Style::default().fg(Color::Green)),
            Span::styled(" SOL", Style::default().fg(Color::Green)),
        ]),
        Line::from(vec![
            Span::styled("Used/Free: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}/{} SOL", used_sol, free_sol)),
        ]),
        Line::from(vec![
            Span::styled("Wallet: ", Style::default().fg(Color::Gray)),
            Span::raw(wallet),
        ]),
    ])
    .wrap(Wrap { trim: true })
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, area);
}

fn render_market_list(frame: &mut Frame, app: &AppState, area: Rect) {
    const NAME_W: usize = 18;
    const PRICE_W: usize = 10;
    const MARGIN_W: usize = 9;
    const STATE_W: usize = 7;
    const PNL_W: usize = 9;
    const UPNL_W: usize = 9;

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Markets (a add, d remove, p pause)");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    let legend = Paragraph::new(Line::from(vec![
        Span::raw("   "),
        Span::styled(
            format!("{:^NAME_W$}", "Name"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:^PRICE_W$}", "Price(SOL)"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:^MARGIN_W$}", "Margin"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:^STATE_W$}", "State"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:^PNL_W$}", "PnL"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("{:^UPNL_W$}", "UPNL"),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    frame.render_widget(legend, chunks[0]);

    let mut state = ListState::default();
    if !app.snapshot.markets.is_empty() {
        state.select(Some(app.selected));
    }

    let items: Vec<ListItem> = if app.snapshot.markets.is_empty() {
        vec![ListItem::new(Line::from(vec![Span::styled(
            "No active markets",
            Style::default().fg(Color::DarkGray),
        )]))]
    } else {
        app.snapshot
            .markets
            .iter()
            .map(|market| {
                let name = market
                    .token_name
                    .as_deref()
                    .map(|name| truncate_middle(name, NAME_W))
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "--".to_string());
                let price = market
                    .last_price
                    .filter(|px| *px > 0.0)
                    .map(|px| format!("{:.8}", px))
                    .unwrap_or_else(|| "--".to_string());
                let margin_sol = format_sol(market.margin_lamports);
                let state = format_engine_state(market.engine_state);
                let pnl_value = market.pnl;
                let pnl_text = format_signed_compact(pnl_value);
                let pnl_color_value = pnl_color(pnl_value);
                let (upnl_text, upnl_color) = match compute_upnl(market) {
                    Some(value) => (format_signed_compact(value), pnl_color(value)),
                    None => ("--".to_string(), Color::DarkGray),
                };

                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{name:^NAME_W$}"),
                        Style::default().fg(Color::White),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{price:^PRICE_W$}"),
                        Style::default().fg(Color::White),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:^MARGIN_W$}", margin_sol),
                        Style::default().fg(Color::Blue),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{state:^STATE_W$}"),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{pnl_text:^PNL_W$}"),
                        Style::default().fg(pnl_color_value),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{upnl_text:^UPNL_W$}"),
                        Style::default().fg(upnl_color),
                    ),
                ]))
            })
            .collect()
    };

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    frame.render_stateful_widget(list, chunks[1], &mut state);
}

fn render_selected_market(frame: &mut Frame, app: &AppState, area: Rect) {
    let lines = if let Some(market) = app.selected_market() {
        let upnl_value = compute_upnl(market);
        let upnl_span = match upnl_value {
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

        let mut lines = vec![
            kv_line(
                "Name",
                Span::raw(
                    market
                        .token_name
                        .as_deref()
                        .map(|name| truncate_middle(name, 42))
                        .unwrap_or_else(|| "--".to_string()),
                ),
            ),
            kv_line("CA", Span::raw(market.token_mint.to_string())),
            kv_line(
                "Mkt Cap",
                Span::raw(format_market_cap(compute_market_cap_usd(
                    market,
                    app.snapshot.sol_price_usd,
                ))),
            ),
            kv_line(
                "State",
                Span::styled(
                    format_engine_state(market.engine_state),
                    Style::default().fg(Color::Yellow),
                ),
            ),
            kv_line(
                "Margin",
                Span::raw(format!("{} SOL", format_sol(market.margin_lamports))),
            ),
            kv_line(
                "Last price",
                Span::raw(
                    market
                        .last_price
                        .map(|px| format!("{:.8}", px))
                        .unwrap_or_else(|| "--".to_string()),
                ),
            ),
            kv_line(
                "Last update",
                Span::raw(
                    market
                        .last_price_ts
                        .map(format_time_ms)
                        .unwrap_or_else(|| "--".to_string()),
                ),
            ),
            kv_line(
                "Paused",
                Span::raw(if market.is_paused { "YES" } else { "NO" }),
            ),
            Line::from(""),
            Line::from(vec![Span::styled(
                "Indicators",
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )]),
        ];

        if market.indicators.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "--",
                Style::default().fg(Color::DarkGray),
            )]));
        } else {
            let max_rows = 8usize;
            for indicator in market.indicators.iter().take(max_rows) {
                lines.push(indicator_line(indicator));
            }
            if market.indicators.len() > max_rows {
                lines.push(Line::from(format!(
                    "... +{} more",
                    market.indicators.len() - max_rows
                )));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Position",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(kv_line(
            "Status",
            Span::raw(if market.open_pos.is_some() {
                "Open"
            } else {
                "None"
            }),
        ));
        lines.push(kv_line("UPNL", upnl_span));
        lines.push(kv_line(
            "Entry",
            Span::raw(
                market
                    .open_pos
                    .map(|pos| format!("{:.8}", pos.entry_px))
                    .unwrap_or_else(|| "--".to_string()),
            ),
        ));
        lines.push(kv_line(
            "Open time",
            Span::raw(
                market
                    .open_pos
                    .map(|pos| format_time_ms(pos.open_time))
                    .unwrap_or_else(|| "--".to_string()),
            ),
        ));
        lines.push(kv_line(
            "Size",
            Span::raw(
                market
                    .open_pos
                    .map(|pos| format!("{:.8}", pos.size))
                    .unwrap_or_else(|| "--".to_string()),
            ),
        ));
        lines.push(kv_line(
            "Realized",
            Span::styled(format!("{:.6}", market.pnl), pnl_style(market.pnl)),
        ));

        lines
    } else {
        vec![Line::from(vec![Span::styled(
            "No market selected",
            Style::default().fg(Color::DarkGray),
        )])]
    };

    let details = Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title("Selected"));
    frame.render_widget(details, area);
}

fn render_footer(frame: &mut Frame, app: &AppState, area: Rect) {
    let footer = Paragraph::new(Line::from(app.status.as_str()))
        .wrap(Wrap { trim: true })
        .block(Block::default().borders(Borders::ALL).title("Logs"));
    frame.render_widget(footer, area);
}

fn render_add_modal(frame: &mut Frame, form: &AddForm) {
    let area = centered_rect(70, 40, frame.area());
    frame.render_widget(Clear, area);

    let mint_style = if form.active == AddField::Mint {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let alloc_style = if form.active == AddField::Alloc {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let strat_style = if form.active == AddField::Strategy {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let strategy_label = STRATEGY_OPTIONS
        .get(form.strategy_idx)
        .map(|opt| opt.label)
        .unwrap_or("--");

    let lines = vec![
        Line::from("Create market"),
        Line::from(""),
        Line::from(vec![
            Span::styled("Mint:   ", Style::default().fg(Color::Gray)),
            Span::styled(form.mint.as_str(), mint_style),
        ]),
        Line::from(vec![
            Span::styled("Margin: ", Style::default().fg(Color::Gray)),
            Span::styled(format!("{} SOL", form.margin_sol), alloc_style),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Strat:  ", Style::default().fg(Color::Gray)),
            Span::styled(strategy_label, strat_style),
        ]),
        Line::from(vec![
            Span::raw("Options: "),
            strategy_options_line(form.strategy_idx),
        ]),
        Line::from(""),
        Line::from("tab/up/down field | <-/-> strategy | enter submit | esc cancel"),
    ];

    let modal = Paragraph::new(lines).wrap(Wrap { trim: true }).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Add Market (margin SOL > 0.0)"),
    );
    frame.render_widget(modal, area);
}

fn strategy_options_line(selected_idx: usize) -> Span<'static> {
    let text = STRATEGY_OPTIONS
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            if i == selected_idx {
                format!("[{}]", opt.label)
            } else {
                opt.label.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" | ");

    Span::styled(text, Style::default().fg(Color::Cyan))
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn kv_line<'a>(label: &'a str, value: Span<'a>) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{label:<11}", label = label),
            Style::default().fg(Color::Gray),
        ),
        value,
    ])
}

fn indicator_line(indicator: &engine_rs::IndicatorData) -> Line<'static> {
    let id = format!("{:?} @ {}", indicator.id.0, indicator.id.1);
    let value = indicator
        .value
        .as_ref()
        .map(|v| truncate_middle(&format!("{:?}", v), 60))
        .unwrap_or_else(|| "--".to_string());

    Line::from(vec![
        Span::styled(id, Style::default().fg(Color::Cyan)),
        Span::raw(" = "),
        Span::raw(value),
    ])
}

fn compute_market_cap_usd(market: &MarketState, sol_price_usd: Option<f64>) -> Option<f64> {
    let raw_supply = market.total_supply?;
    let supply_decimals = market.supply_decimals?;
    let token_price_sol = market.last_price?;
    let sol_price_usd = sol_price_usd?;

    if token_price_sol <= 0.0 || sol_price_usd <= 0.0 {
        return None;
    }

    let supply = raw_supply as f64 / 10f64.powi(supply_decimals as i32);
    Some(supply * token_price_sol * sol_price_usd)
}

fn format_signed_compact(value: f64) -> String {
    if !value.is_finite() {
        return "--".to_string();
    }

    if value.abs() < 0.000_000_000_5 {
        return "0.000000".to_string();
    }

    format!("{:+.6}", value)
}

fn pnl_color(value: f64) -> Color {
    if value > 0.0 {
        Color::Green
    } else if value < 0.0 {
        Color::Red
    } else {
        Color::White
    }
}

fn format_market_cap(value: Option<f64>) -> String {
    let Some(value) = value else {
        return "--".to_string();
    };
    if !value.is_finite() || value <= 0.0 {
        return "--".to_string();
    }

    if value >= 1_000_000_000_000.0 {
        return format!("{:.1}T", value / 1_000_000_000_000.0);
    }
    if value >= 1_000_000_000.0 {
        return format!("{:.1}B", value / 1_000_000_000.0);
    }
    if value >= 1_000_000.0 {
        return format!("{:.1}M", value / 1_000_000.0);
    }
    if value >= 1_000.0 {
        return format!("{:.1}K", value / 1_000.0);
    }
    format!("{:.0}", value)
}

fn format_sol_usd(value: Option<f64>) -> String {
    match value {
        Some(v) if v.is_finite() && v > 0.0 => format!("{:.4}", v),
        _ => "--".to_string(),
    }
}

fn compute_upnl(market: &MarketState) -> Option<f64> {
    let pos = market.open_pos?;
    let last = market.last_price?;
    Some((last - pos.entry_px) * pos.size)
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

fn format_time_ms(ts_ms: u64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts_ms as i64)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "--".to_string())
}

fn format_sol(lamports: u64) -> String {
    let sol = lamports as f64 / LAMPORTS_PER_SOL as f64;
    format!("{:.6}", sol)
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

fn pnl_style(pnl: f64) -> Style {
    if pnl > 0.0 {
        Style::default().fg(Color::Green)
    } else if pnl < 0.0 {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::White)
    }
}

fn copy_to_clipboard_osc52(text: &str) -> Result<()> {
    let encoded = STANDARD.encode(text.as_bytes());
    let sequence = format!("\u{1b}]52;c;{}\u{7}", encoded);
    let mut stdout = io::stdout();
    stdout.write_all(sequence.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn init_logger() {
    let log_path = std::env::var("BOT_LOG").unwrap_or_else(|_| "/tmp/bot.log".to_string());
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));

    if let Ok(file) = OpenOptions::new().create(true).append(true).open(log_path) {
        builder.target(env_logger::Target::Pipe(Box::new(file)));
    }

    let _ = builder.try_init();
}
