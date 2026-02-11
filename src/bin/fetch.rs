use engine_rs::birdeye::{BirdeyeClient, QuoteCurrency};
use engine_rs::exec::Executor;
use engine_rs::info::StreamManager;
use engine_rs::{
    BalanceSync, EngineCommand, ExecParams, IndicatorKind, MarketCommand, SOL_MINT, SignalEngine,
    Strategy, TimeFrame,
};
use solana_client::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::signature::Signer;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use engine_rs::helpers::read_keypair_from_file;

const MAINNET_RPC: &str = "https://api.mainnet-beta.solana.com";

const SLIPPAGE_BPS: u16 = 400;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv::dotenv().ok();
    env_logger::init();
    let birdeye_key = std::env::var("BIRDEYE_API_KEY").unwrap();
    let client = BirdeyeClient::new(birdeye_key);

    let keypair = read_keypair_from_file("./keypair.json")?;

    let token_str = "E2HNWS5L6gwmtC9SZPpRq6Yp3V5gzotFrL3dAEP2pump";
    let candles = client
        .fetch_candles(token_str, TimeFrame::Min1, QuoteCurrency::Sol, 1000)
        .await?;
    //dbg!(&candles);

    let rpc_url = std::env::var("RPC_URL").unwrap_or_else(|_| MAINNET_RPC.to_string());
    let rpc_client = Arc::new(RpcClient::new(rpc_url));

    let (engine_tx, engine_rx) = tokio::sync::mpsc::unbounded_channel();
    let (trade_tx, trade_rx) = flume::bounded(0);
    let (market_tx, mut market_rx) = tokio::sync::mpsc::channel(128);

    let indicators = vec![(IndicatorKind::Rsi(12), TimeFrame::Min1)];

    let balance = BalanceSync::new(Arc::clone(&rpc_client), keypair.pubkey()).fetch_balance()?;
    let exec_params = ExecParams::new(balance);

    let mut engine = SignalEngine::new(
        Some(indicators),
        Strategy::RsiClassic,
        engine_rx,
        None,
        trade_tx,
        exec_params,
    )
    .await;

    engine.load(TimeFrame::Min1, candles.clone()).await;

    let sol_mint = Pubkey::from_str(SOL_MINT)?;
    let token_mint = Pubkey::from_str(token_str)?;

    let wallet = Arc::new(keypair);
    let executor = Executor::new(
        trade_rx,
        Some(market_tx),
        rpc_client,
        wallet,
        sol_mint,
        token_mint,
        SLIPPAGE_BPS,
    )?;

    tokio::spawn(async move {
        executor.run().await;
    });

    let engine_tx_market = engine_tx.clone();
    tokio::spawn(async move {
        while let Some(cmd) = market_rx.recv().await {
            match cmd {
                MarketCommand::SwapFill(fill) => {
                    log::info!(
                        "MARKET: swap sig={} in={} out={} in_amount={} out_amount={}",
                        fill.signature,
                        fill.input_mint,
                        fill.output_mint,
                        fill.in_amount,
                        fill.out_amount
                    );
                }
                MarketCommand::Trade(trade) => {
                    log::info!(
                        "MARKET: trade side={:?} size={:.8} pnl={:.8} open_px={:.8} close_px={:.8}",
                        trade.side,
                        trade.size,
                        trade.pnl,
                        trade.open.price,
                        trade.close.price
                    );
                }
                MarketCommand::OpenPosition(pos) => {
                    log::info!("MARKET: open position update: {:?}", pos);
                    let _ = engine_tx_market.send(EngineCommand::UpdateExecParams(
                        engine_rs::ExecParam::OpenPosition(pos),
                    ));
                }
                MarketCommand::IndicatorData(data) => {
                    log::info!("MARKET: indicator data len={}", data.len());
                }
                MarketCommand::EngineState(state) => {
                    log::info!("MARKET: engine state {:?}", state);
                }
            }
        }
    });

    tokio::spawn(async move {
        engine.start().await;
    });

    let mint = token_mint;
    let mut manager = StreamManager::new()?;

    loop {
        let (sub_id, mut rx) = match manager.subscribe_candles(mint).await {
            Ok(sub) => sub,
            Err(err) => {
                log::warn!("candle subscribe failed: {}", err);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        log::info!("candle stream subscribed: {}", sub_id);

        while let Some(candle) = rx.recv().await {
            let _ = engine_tx.send(EngineCommand::UpdatePrice(candle));
        }

        log::warn!("candle stream ended; resubscribing");
        let _ = manager.unsubscribe_candles(sub_id);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
