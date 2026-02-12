use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use flume::{Sender as FlumeSender, bounded};
use log::{info, warn};
use solana_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use crate::birdeye::{BirdeyeClient, QuoteCurrency};
use crate::exec::Executor;
use crate::info::StreamManager;
use crate::{
    BalanceSync, EngineCommand, ExecCommand, ExecControl, ExecParam, ExecParams, MarketCommand,
    Price, SOL_MINT, SignalEngine, Strategy, TimeFrame,
};

const RECONNECT_DELAY_SECS: u64 = 2;
const MAINNET_RPC: &str = "https://api.mainnet-beta.solana.com";

#[derive(Clone, Debug)]
pub struct MarketConfig {
    pub token_mint: Pubkey,
    pub strategy: Strategy,
    pub quote_currency: QuoteCurrency,
    pub bootstrap_candles: u64,
    pub slippage_bps: u16,
}

impl MarketConfig {
    pub fn new(token_mint: Pubkey) -> Self {
        Self {
            token_mint,
            strategy: Strategy::RsiClassic,
            quote_currency: QuoteCurrency::Sol,
            bootstrap_candles: 1000,
            slippage_bps: 400,
        }
    }
}

#[derive(Clone, Debug)]
pub enum MarketEvent {
    Price {
        token_mint: Pubkey,
        price: Price,
    },
    Command {
        token_mint: Pubkey,
        command: MarketCommand,
    },
}

pub struct Market {
    token_mint: Pubkey,
    engine_tx: mpsc::UnboundedSender<EngineCommand>,
    exec_tx: FlumeSender<ExecCommand>,
    tasks: Vec<JoinHandle<()>>,
}

impl Market {
    pub async fn spawn(
        cfg: MarketConfig,
        rpc_client: Arc<RpcClient>,
        wallet: Arc<Keypair>,
        birdeye: &BirdeyeClient,
        event_tx: Option<UnboundedSender<MarketEvent>>,
    ) -> Result<Self> {
        let token_str = cfg.token_mint.to_string();
        let active_tfs: HashSet<TimeFrame> = cfg
            .strategy
            .indicators()
            .into_iter()
            .map(|id| id.1)
            .collect();

        let mut bootstrap_data = Vec::with_capacity(active_tfs.len());
        for tf in active_tfs {
            let candles = birdeye
                .fetch_candles(&token_str, tf, cfg.quote_currency, cfg.bootstrap_candles)
                .await?;
            bootstrap_data.push((tf, candles));
        }

        let (engine_tx, engine_rx) = mpsc::unbounded_channel();
        let (trade_tx, trade_rx) = bounded(0);
        let (market_tx, mut market_rx) = mpsc::channel(256);
        let exec_tx = trade_tx.clone();

        let balance = BalanceSync::new(Arc::clone(&rpc_client), wallet.pubkey()).fetch_balance()?;
        let exec_params = ExecParams::new(balance);

        let mut engine = SignalEngine::new(
            None,
            cfg.strategy,
            engine_rx,
            Some(market_tx.clone()),
            trade_tx,
            exec_params,
        )
        .await;
        for (tf, candles) in bootstrap_data {
            engine.load(tf, candles).await;
        }

        let base_mint = Pubkey::from_str(SOL_MINT)?;
        let executor_rpc_url = std::env::var("EXECUTOR_RPC_URL")
            .ok()
            .or_else(|| std::env::var("RPC_URL").ok())
            .unwrap_or_else(|| MAINNET_RPC.to_string());
        let executor_rpc = Arc::new(RpcClient::new(executor_rpc_url));
        let executor = Executor::new(
            trade_rx,
            Some(market_tx),
            executor_rpc,
            Arc::clone(&wallet),
            base_mint,
            cfg.token_mint,
            cfg.slippage_bps,
        )?;

        let token_mint = cfg.token_mint;
        let engine_task = tokio::spawn(async move {
            engine.start().await;
        });

        let executor_task = tokio::spawn(async move {
            executor.run().await;
        });

        let engine_tx_market = engine_tx.clone();
        let event_tx_market = event_tx.clone();
        let relay_task = tokio::spawn(async move {
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
                if let Some(tx) = &event_tx_market {
                    let _ = tx.send(MarketEvent::Command {
                        token_mint,
                        command: cmd,
                    });
                }
            }
        });

        let engine_tx_price = engine_tx.clone();
        let event_tx_price = event_tx;
        let stream_task = tokio::spawn(async move {
            let mut manager = match StreamManager::new() {
                Ok(manager) => manager,
                Err(err) => {
                    warn!("stream manager init failed for {}: {}", token_mint, err);
                    return;
                }
            };

            loop {
                let (sub_id, mut rx) = match manager.subscribe_candles(token_mint).await {
                    Ok(sub) => sub,
                    Err(err) => {
                        warn!("candle subscribe failed for {}: {}", token_mint, err);
                        tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
                        continue;
                    }
                };

                info!("candle stream subscribed: {} ({})", sub_id, token_mint);
                while let Some(candle) = rx.recv().await {
                    let _ = engine_tx_price.send(EngineCommand::UpdatePrice(candle));
                    if let Some(tx) = &event_tx_price {
                        let _ = tx.send(MarketEvent::Price {
                            token_mint,
                            price: candle,
                        });
                    }
                }

                warn!("candle stream ended; resubscribing ({})", token_mint);
                let _ = manager.unsubscribe_candles(sub_id);
                tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
            }
        });

        Ok(Self {
            token_mint: cfg.token_mint,
            engine_tx,
            exec_tx,
            tasks: vec![engine_task, executor_task, relay_task, stream_task],
        })
    }

    pub fn token_mint(&self) -> Pubkey {
        self.token_mint
    }

    pub fn send_engine_command(&self, cmd: EngineCommand) -> Result<()> {
        self.engine_tx
            .send(cmd)
            .map_err(|err| anyhow::anyhow!("engine command send failed: {err}"))
    }

    pub async fn send_exec_command(&self, cmd: ExecCommand) -> Result<()> {
        self.exec_tx
            .send_async(cmd)
            .await
            .map_err(|err| anyhow::anyhow!("exec command send failed: {err}"))
    }

    pub fn update_margin(&self, margin_lamports: u64) -> Result<()> {
        self.send_engine_command(EngineCommand::UpdateExecParams(ExecParam::Margin(
            margin_lamports,
        )))
    }

    pub async fn pause(&self) -> Result<()> {
        self.send_exec_command(ExecCommand::Control(ExecControl::Pause))
            .await
    }

    pub async fn resume(&self) -> Result<()> {
        self.send_exec_command(ExecCommand::Control(ExecControl::Resume))
            .await
    }

    pub async fn force_close(&self) -> Result<()> {
        self.send_exec_command(ExecCommand::Control(ExecControl::ForceClose))
            .await
    }

    pub async fn kill(&self) -> Result<()> {
        self.send_exec_command(ExecCommand::Control(ExecControl::Kill))
            .await
    }

    pub fn stop(mut self) {
        let _ = self.engine_tx.send(EngineCommand::Stop);
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}
