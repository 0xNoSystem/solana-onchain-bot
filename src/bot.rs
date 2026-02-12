use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use solana_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::native_token::LAMPORTS_PER_SOL;
use solana_sdk::signature::{Keypair, Signer};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::interval;

use crate::birdeye::BirdeyeClient;
use crate::margin::{AssetMargin, MarginAllocation, MarginBook};
use crate::sol_price::{SolPriceStreamConfig, spawn_sol_price_stream};
use crate::{EngineView, IndicatorData, Market, MarketConfig, MarketEvent, OpenPosInfo, TradeInfo};

const MARGIN_SYNC_INTERVAL_SECS: u64 = 3;
const CLOSE_GRACE_SECS: u64 = 2;
const TOKEN_2022_ACCOUNT_TYPE_OFFSET: usize = 165;
const TOKEN_2022_TLV_START: usize = TOKEN_2022_ACCOUNT_TYPE_OFFSET + 1;
const TOKEN_2022_ACCOUNT_TYPE_MINT: u8 = 1;
const TOKEN_2022_EXT_UNINITIALIZED: u16 = 0;
const TOKEN_2022_EXT_TOKEN_METADATA: u16 = 19;
const METAPLEX_METADATA_PROGRAM_ID: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";

#[derive(Clone, Debug)]
pub struct AddMarketInfo {
    pub market_cfg: MarketConfig,
    pub margin_alloc: MarginAllocation,
}

#[derive(Clone, Debug)]
pub struct MarketState {
    pub token_mint: Pubkey,
    pub token_name: Option<String>,
    pub total_supply: Option<u64>,
    pub supply_decimals: Option<u8>,
    pub margin_lamports: u64,
    pub engine_state: EngineView,
    pub open_pos: Option<OpenPosInfo>,
    pub indicators: Vec<IndicatorData>,
    pub last_price: Option<f64>,
    pub last_price_ts: Option<u64>,
    pub pnl: f64,
    pub trades: HashMap<String, TradeInfo>,
    pub is_paused: bool,
}

#[derive(Clone, Debug, Default)]
pub struct BotSnapshot {
    pub total_on_chain_lamports: u64,
    pub used_lamports: u64,
    pub free_lamports: u64,
    pub sol_price_usd: Option<f64>,
    pub markets: Vec<MarketState>,
}

#[derive(Debug)]
pub enum BotCommand {
    AddMarket {
        info: AddMarketInfo,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveMarket(Pubkey),
    PauseMarket(Pubkey),
    ResumeMarket(Pubkey),
    UpdateMargin(AssetMargin),
    PauseAll,
    ResumeAll,
    CloseAll,
    GetSnapshot(oneshot::Sender<BotSnapshot>),
    Kill,
}

pub struct Bot {
    rpc_client: Arc<RpcClient>,
    wallet: Arc<Keypair>,
    birdeye: BirdeyeClient,
    markets: HashMap<Pubkey, Market>,
    session: HashMap<Pubkey, MarketState>,
    margin_book: MarginBook,
    cmd_rx: Receiver<BotCommand>,
    market_event_rx: UnboundedReceiver<MarketEvent>,
    market_event_tx: UnboundedSender<MarketEvent>,
    sol_price_usd: Option<f64>,
    sol_price_tx: UnboundedSender<f64>,
    sol_price_rx: UnboundedReceiver<f64>,
    sol_price_task: Option<JoinHandle<()>>,
}

impl Bot {
    pub fn new(
        rpc_client: Arc<RpcClient>,
        wallet: Arc<Keypair>,
        birdeye: BirdeyeClient,
    ) -> (Self, Sender<BotCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<BotCommand>(64);
        let (market_event_tx, market_event_rx) = mpsc::unbounded_channel::<MarketEvent>();
        let (sol_price_tx, sol_price_rx) = mpsc::unbounded_channel::<f64>();
        let margin_book = MarginBook::new(Arc::clone(&rpc_client), wallet.pubkey());

        (
            Self {
                rpc_client,
                wallet,
                birdeye,
                markets: HashMap::new(),
                session: HashMap::new(),
                margin_book,
                cmd_rx,
                market_event_rx,
                market_event_tx,
                sol_price_usd: None,
                sol_price_tx,
                sol_price_rx,
                sol_price_task: None,
            },
            cmd_tx,
        )
    }

    pub async fn start(mut self) -> Result<()> {
        if self.sol_price_task.is_none() {
            let cfg = SolPriceStreamConfig::from_env();
            self.sol_price_task = Some(spawn_sol_price_stream(cfg, self.sol_price_tx.clone()));
        }
        let mut sync_tick = interval(Duration::from_secs(MARGIN_SYNC_INTERVAL_SECS));
        let _ = self.margin_book.sync();

        loop {
            tokio::select! {
                Some(event) = self.market_event_rx.recv() => {
                    self.handle_market_event(event);
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    if self.handle_cmd(cmd).await? {
                        break;
                    }
                }
                Some(sol_price) = self.sol_price_rx.recv() => {
                    self.sol_price_usd = Some(sol_price);
                }
                _ = sync_tick.tick() => {
                    if let Err(err) = self.margin_book.sync() {
                        log::warn!("margin sync failed: {}", err);
                    }
                }
                else => break,
            }
        }

        if let Some(task) = self.sol_price_task.take() {
            task.abort();
        }

        Ok(())
    }

    async fn handle_cmd(&mut self, cmd: BotCommand) -> Result<bool> {
        match cmd {
            BotCommand::AddMarket { info, reply } => {
                let add_result = self.add_market(info).await;
                if let Err(err) = &add_result {
                    log::warn!("add market failed: {}", err);
                }
                let _ = reply.send(add_result);
            }
            BotCommand::RemoveMarket(token_mint) => {
                if let Err(err) = self.remove_market(token_mint).await {
                    log::warn!("remove market failed: {}", err);
                }
            }
            BotCommand::PauseMarket(token_mint) => {
                if let Some(market) = self.markets.get(&token_mint) {
                    if let Err(err) = market.pause().await {
                        log::warn!("pause failed for {}: {}", token_mint, err);
                    }
                }
            }
            BotCommand::ResumeMarket(token_mint) => {
                if let Some(market) = self.markets.get(&token_mint) {
                    if let Err(err) = market.resume().await {
                        log::warn!("resume failed for {}: {}", token_mint, err);
                    }
                }
            }
            BotCommand::UpdateMargin(update) => {
                if let Err(err) = self.update_margin(update).await {
                    log::warn!("update margin failed: {}", err);
                }
            }
            BotCommand::PauseAll => {
                for market in self.markets.values() {
                    if let Err(err) = market.pause().await {
                        log::warn!("pause all failed: {}", err);
                    }
                }
            }
            BotCommand::ResumeAll => {
                for market in self.markets.values() {
                    if let Err(err) = market.resume().await {
                        log::warn!("resume all failed: {}", err);
                    }
                }
            }
            BotCommand::CloseAll => {
                self.close_all().await;
            }
            BotCommand::GetSnapshot(reply) => {
                let _ = reply.send(self.snapshot());
            }
            BotCommand::Kill => {
                self.close_all().await;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn add_market(&mut self, info: AddMarketInfo) -> Result<()> {
        let token_mint = info.market_cfg.token_mint;
        if self.markets.contains_key(&token_mint) {
            return Err(anyhow!("market already exists: {}", token_mint));
        }

        let margin_lamports = self.margin_book.allocate(token_mint, info.margin_alloc)?;
        let (token_name, total_supply, supply_decimals) =
            fetch_token_details(self.rpc_client.as_ref(), &token_mint);

        let market = match Market::spawn(
            info.market_cfg.clone(),
            Arc::clone(&self.rpc_client),
            Arc::clone(&self.wallet),
            &self.birdeye,
            Some(self.market_event_tx.clone()),
        )
        .await
        {
            Ok(market) => market,
            Err(err) => {
                self.margin_book.remove(&token_mint);
                return Err(err);
            }
        };

        if let Err(err) = market.update_margin(margin_lamports) {
            self.margin_book.remove(&token_mint);
            market.stop();
            return Err(err);
        }

        self.session.insert(
            token_mint,
            MarketState {
                token_mint,
                token_name,
                total_supply,
                supply_decimals,
                margin_lamports,
                engine_state: EngineView::Idle,
                open_pos: None,
                indicators: Vec::new(),
                last_price: None,
                last_price_ts: None,
                pnl: 0.0,
                trades: HashMap::new(),
                is_paused: false,
            },
        );
        self.markets.insert(token_mint, market);
        Ok(())
    }

    async fn remove_market(&mut self, token_mint: Pubkey) -> Result<()> {
        if let Some(market) = self.markets.remove(&token_mint) {
            let _ = market.pause().await;
            let _ = market.force_close().await;
            let _ = market.kill().await;
            tokio::time::sleep(Duration::from_secs(CLOSE_GRACE_SECS)).await;
            market.stop();
        }
        self.margin_book.remove(&token_mint);
        self.session.remove(&token_mint);
        Ok(())
    }

    async fn update_margin(&mut self, update: AssetMargin) -> Result<()> {
        let (token_mint, requested_margin) = update;
        let state = self
            .session
            .get(&token_mint)
            .ok_or_else(|| anyhow!("market not found: {}", token_mint))?;
        if matches!(
            state.engine_state,
            EngineView::Open | EngineView::Opening | EngineView::Closing
        ) {
            return Err(anyhow!(
                "margin update blocked while market has open/active position: {}",
                token_mint
            ));
        }

        let new_margin = self
            .margin_book
            .update_asset((token_mint, requested_margin))?;
        let market = self
            .markets
            .get(&token_mint)
            .ok_or_else(|| anyhow!("market handle not found: {}", token_mint))?;
        market.update_margin(new_margin)?;

        if let Some(state) = self.session.get_mut(&token_mint) {
            state.margin_lamports = new_margin;
        }
        Ok(())
    }

    async fn close_all(&mut self) {
        let markets = std::mem::take(&mut self.markets);
        for (token_mint, market) in markets {
            let _ = market.pause().await;
            let _ = market.force_close().await;
            let _ = market.kill().await;
            tokio::time::sleep(Duration::from_secs(CLOSE_GRACE_SECS)).await;
            market.stop();
            self.margin_book.remove(&token_mint);
        }
        self.session.clear();
        self.margin_book.reset();
    }

    fn handle_market_event(&mut self, event: MarketEvent) {
        match event {
            MarketEvent::Price { token_mint, price } => {
                if let Some(state) = self.session.get_mut(&token_mint) {
                    state.last_price = Some(price.close);
                    state.last_price_ts = Some(price.close_time);
                }
            }
            MarketEvent::Command {
                token_mint,
                command,
            } => {
                let mut pnl_to_compound: Option<f64> = None;
                if let Some(state) = self.session.get_mut(&token_mint) {
                    match command {
                        crate::MarketCommand::OpenPosition(pos) => {
                            state.open_pos = pos;
                        }
                        crate::MarketCommand::EngineState(view) => {
                            state.engine_state = view;
                            if matches!(view, EngineView::Idle) {
                                state.open_pos = None;
                            }
                        }
                        crate::MarketCommand::IndicatorData(indicators) => {
                            state.indicators = indicators;
                        }
                        crate::MarketCommand::ExecutorPaused(paused) => {
                            state.is_paused = paused;
                        }
                        crate::MarketCommand::OpenFailed(reason) => {
                            log::warn!("market {} open failed: {}", token_mint, reason);
                            state.open_pos = None;
                            state.engine_state = EngineView::Idle;
                        }
                        crate::MarketCommand::Trade(trade) => {
                            let trade_key = if trade.close_tx_hash.is_empty() {
                                format!("close-{}", trade.close.time)
                            } else {
                                trade.close_tx_hash.clone()
                            };
                            if !state.trades.contains_key(&trade_key) {
                                state.trades.insert(trade_key, trade.clone());
                                state.pnl += trade.pnl;
                                pnl_to_compound = Some(trade.pnl);
                            }
                            // A Trade event is emitted on finalized full close.
                            // Clear stale position state even if an OpenPosition(None)
                            // message is delayed or dropped in relay paths.
                            state.open_pos = None;
                            state.engine_state = EngineView::Idle;
                        }
                        _ => {}
                    }
                }
                if let Some(pnl) = pnl_to_compound {
                    self.compound_margin_from_trade_pnl(token_mint, pnl);
                }
            }
        }
    }

    fn compound_margin_from_trade_pnl(&mut self, token_mint: Pubkey, trade_pnl_sol: f64) {
        if !trade_pnl_sol.is_finite() {
            log::warn!(
                "skipping margin compounding for {} due to non-finite trade pnl: {}",
                token_mint,
                trade_pnl_sol
            );
            return;
        }

        let Some(state) = self.session.get(&token_mint) else {
            return;
        };
        let current_margin = state.margin_lamports;
        let delta_lamports = sol_to_lamports_delta(trade_pnl_sol);
        if delta_lamports == 0 {
            return;
        }

        let proposed_margin = apply_lamports_delta(current_margin, delta_lamports);
        if proposed_margin == current_margin {
            return;
        }

        let previous_margin = match self.margin_book.set_asset(token_mint, proposed_margin) {
            Ok(_) => current_margin,
            Err(err) => {
                log::warn!(
                    "failed to update margin book after trade for {}: {}",
                    token_mint,
                    err
                );
                return;
            }
        };

        if let Some(market) = self.markets.get(&token_mint) {
            if let Err(err) = market.update_margin(proposed_margin) {
                log::warn!(
                    "failed to propagate compounded margin to engine for {}: {}",
                    token_mint,
                    err
                );
                let _ = self.margin_book.set_asset(token_mint, previous_margin);
                return;
            }
        }

        if let Some(state) = self.session.get_mut(&token_mint) {
            state.margin_lamports = proposed_margin;
        }
    }

    fn snapshot(&self) -> BotSnapshot {
        BotSnapshot {
            total_on_chain_lamports: self.margin_book.total_on_chain_lamports,
            used_lamports: self.margin_book.used(),
            free_lamports: self.margin_book.free(),
            sol_price_usd: self.sol_price_usd,
            markets: self.session.values().cloned().collect(),
        }
    }
}

fn fetch_token_details(
    rpc: &RpcClient,
    mint: &Pubkey,
) -> (Option<String>, Option<u64>, Option<u8>) {
    let token_name = match fetch_token_name(rpc, mint) {
        Ok(name) => name,
        Err(err) => {
            log::warn!("token name lookup failed for {}: {}", mint, err);
            None
        }
    };

    let (total_supply, supply_decimals) = match rpc.get_token_supply(mint) {
        Ok(supply) => (supply.amount.parse::<u64>().ok(), Some(supply.decimals)),
        Err(err) => {
            log::warn!("token supply lookup failed for {}: {}", mint, err);
            (None, None)
        }
    };

    (token_name, total_supply, supply_decimals)
}

fn fetch_token_name(rpc: &RpcClient, mint: &Pubkey) -> Result<Option<String>> {
    if let Some(name) = fetch_token_2022_metadata_name(rpc, mint)? {
        return Ok(Some(name));
    }
    fetch_metaplex_metadata_name(rpc, mint)
}

fn fetch_token_2022_metadata_name(rpc: &RpcClient, mint: &Pubkey) -> Result<Option<String>> {
    let mint_data = rpc.get_account_data(mint)?;
    parse_token_metadata_name_from_mint_data(&mint_data)
}

fn fetch_metaplex_metadata_name(rpc: &RpcClient, mint: &Pubkey) -> Result<Option<String>> {
    let metadata_program = Pubkey::from_str(METAPLEX_METADATA_PROGRAM_ID)?;
    let (metadata_pda, _) = Pubkey::find_program_address(
        &[b"metadata", metadata_program.as_ref(), mint.as_ref()],
        &metadata_program,
    );

    let metadata_data = match rpc.get_account_data(&metadata_pda) {
        Ok(data) => data,
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("could not find account") {
                return Ok(None);
            }
            return Err(anyhow!(err));
        }
    };

    parse_metaplex_name_from_metadata_account(&metadata_data).map(Some)
}

fn parse_token_metadata_name_from_mint_data(mint_data: &[u8]) -> Result<Option<String>> {
    if mint_data.len() <= TOKEN_2022_TLV_START {
        return Ok(None);
    }
    if mint_data.len() <= TOKEN_2022_ACCOUNT_TYPE_OFFSET {
        return Ok(None);
    }
    if mint_data[TOKEN_2022_ACCOUNT_TYPE_OFFSET] != TOKEN_2022_ACCOUNT_TYPE_MINT {
        return Ok(None);
    }

    let mut idx = TOKEN_2022_TLV_START;
    while idx + 4 <= mint_data.len() {
        let ext_type = u16::from_le_bytes([mint_data[idx], mint_data[idx + 1]]);
        let len = u16::from_le_bytes([mint_data[idx + 2], mint_data[idx + 3]]) as usize;
        idx += 4;

        if ext_type == TOKEN_2022_EXT_UNINITIALIZED {
            break;
        }
        if idx + len > mint_data.len() {
            return Err(anyhow!("token metadata tlv length out of bounds"));
        }
        if ext_type == TOKEN_2022_EXT_TOKEN_METADATA {
            let value = &mint_data[idx..idx + len];
            let name = parse_token_2022_name(value)?;
            return Ok(Some(name));
        }
        idx += len;
    }

    Ok(None)
}

fn sol_to_lamports_delta(sol_delta: f64) -> i128 {
    (sol_delta * LAMPORTS_PER_SOL as f64).round() as i128
}

fn apply_lamports_delta(current: u64, delta: i128) -> u64 {
    if delta >= 0 {
        current.saturating_add(delta as u64)
    } else {
        current.saturating_sub((-delta) as u64)
    }
}

fn parse_token_2022_name(data: &[u8]) -> Result<String> {
    let mut offset = 0usize;
    let header_len = 32 + 32;
    if data.len() < header_len + 4 {
        return Err(anyhow!("token metadata too short"));
    }
    offset += header_len;
    let name = read_borsh_string(data, &mut offset)?;
    Ok(clean_metadata_text(name))
}

fn parse_metaplex_name_from_metadata_account(data: &[u8]) -> Result<String> {
    let mut offset = 0usize;
    let account_header_len = 1 + 32 + 32;
    if data.len() < account_header_len + 4 {
        return Err(anyhow!("metaplex metadata account too short"));
    }
    offset += account_header_len;
    let name = read_borsh_string(data, &mut offset)?;
    Ok(clean_metadata_text(name))
}

fn read_borsh_string(data: &[u8], offset: &mut usize) -> Result<String> {
    if *offset + 4 > data.len() {
        return Err(anyhow!("string length out of bounds"));
    }
    let len = u32::from_le_bytes(
        data[*offset..*offset + 4]
            .try_into()
            .expect("slice length checked"),
    ) as usize;
    *offset += 4;
    if *offset + len > data.len() {
        return Err(anyhow!("string data out of bounds"));
    }
    let bytes = &data[*offset..*offset + len];
    *offset += len;
    let value = std::str::from_utf8(bytes)?;
    Ok(value.to_string())
}

fn clean_metadata_text(value: String) -> String {
    value.trim_end_matches(char::from(0)).trim().to_string()
}
