use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use flume::Receiver;
use log::{info, warn};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_types::config::{RpcTransactionConfig, UiTransactionEncoding};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, TransactionConfirmationStatus,
    UiMessage, UiTransactionStatusMeta, UiTransactionTokenBalance,
};
use tokio::sync::mpsc::Sender as TokioSender;
use tokio::time::interval;

use crate::helpers::{get_time_now, token_balance};
use crate::{
    ExecCommand, ExecControl, FillInfo, MarketCommand, OpenPosInfo, PositionOp, SOL_MINT, SwapFill,
    TradeInfo, TradeSide,
};

use super::jupiter::JupiterClient;

const POLL_INTERVAL_MS: u64 = 800;
const MIN_ASSET_DUST_RAW: u64 = 1;

#[derive(Clone, Debug)]
struct PositionState {
    open_time: u64,
    open_tx_hash: Option<String>,
    size: f64,
    entry_px: f64,
    open_base_raw: u64,
    open_asset_raw: u64,
    close_base_trade_raw: u64,
    close_base_gross_raw: u64,
    close_asset_raw: u64,
    fees_raw: u64,
}

impl PositionState {
    fn new(
        open_time: u64,
        open_tx_hash: Option<String>,
        size: f64,
        entry_px: f64,
        open_base_raw: u64,
        open_asset_raw: u64,
    ) -> Self {
        Self {
            open_time,
            open_tx_hash,
            size,
            entry_px,
            open_base_raw,
            open_asset_raw,
            close_base_trade_raw: 0,
            close_base_gross_raw: 0,
            close_asset_raw: 0,
            fees_raw: 0,
        }
    }

    fn apply_open_fill(
        &mut self,
        size_delta: f64,
        fill_price: f64,
        base_raw: u64,
        asset_raw: u64,
        fee_raw: u64,
    ) {
        let new_size = self.size + size_delta;
        if new_size > 0.0 {
            self.entry_px = (self.entry_px * self.size + fill_price * size_delta) / new_size;
            self.size = new_size;
        }
        self.open_base_raw = self.open_base_raw.saturating_add(base_raw);
        self.open_asset_raw = self.open_asset_raw.saturating_add(asset_raw);
        self.fees_raw = self.fees_raw.saturating_add(fee_raw);
    }

    fn apply_close_fill(
        &mut self,
        size_delta: f64,
        base_trade_raw: u64,
        base_gross_raw: u64,
        asset_raw: u64,
        fee_raw: u64,
    ) -> bool {
        self.size = (self.size - size_delta).max(0.0);
        self.close_base_trade_raw = self.close_base_trade_raw.saturating_add(base_trade_raw);
        self.close_base_gross_raw = self.close_base_gross_raw.saturating_add(base_gross_raw);
        self.close_asset_raw = self.close_asset_raw.saturating_add(asset_raw);
        self.fees_raw = self.fees_raw.saturating_add(fee_raw);
        self.size <= 0.0
    }

    fn open_pos_info(&self) -> OpenPosInfo {
        OpenPosInfo {
            size: self.size,
            entry_px: self.entry_px,
            open_time: self.open_time,
        }
    }

    fn open_size(&self, asset_decimals: u8) -> f64 {
        amount_to_ui(self.open_asset_raw, asset_decimals)
    }

    fn open_base(&self, base_decimals: u8) -> f64 {
        amount_to_ui(self.open_base_raw, base_decimals)
    }

    fn close_base(&self, base_decimals: u8) -> f64 {
        amount_to_ui(self.close_base_trade_raw, base_decimals)
    }

    fn close_price(&self, base_decimals: u8, asset_decimals: u8) -> f64 {
        let base_ui = self.close_base(base_decimals);
        let asset_ui = amount_to_ui(self.close_asset_raw, asset_decimals);
        if asset_ui == 0.0 {
            0.0
        } else {
            base_ui / asset_ui
        }
    }

    fn pnl(&self, base_decimals: u8) -> f64 {
        let close_gross = amount_to_ui(self.close_base_gross_raw, base_decimals);
        let open = self.open_base(base_decimals);
        let fees = amount_to_ui(self.fees_raw, base_decimals);
        close_gross - open - fees
    }

    fn open_fill_info(&self, base_decimals: u8, asset_decimals: u8) -> FillInfo {
        let price = if self.size > 0.0 {
            self.entry_px
        } else {
            let base_ui = self.open_base(base_decimals);
            let asset_ui = amount_to_ui(self.open_asset_raw, asset_decimals);
            if asset_ui == 0.0 {
                0.0
            } else {
                base_ui / asset_ui
            }
        };
        FillInfo {
            time: self.open_time,
            price,
            side: TradeSide::Buy,
            in_amount: self.open_base_raw,
            out_amount: self.open_asset_raw,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingSwap {
    intent: PositionOp,
    input_mint: Pubkey,
    output_mint: Pubkey,
    input_amount: u64,
    expected_out_amount: Option<u64>,
}

#[derive(Clone, Debug)]
struct SwapFillAmounts {
    in_amount: u64,
    out_amount: u64,
    in_decimals: u8,
    out_decimals: u8,
    fee_lamports: u64,
    sol_wallet_delta: Option<i128>,
}

struct BaseAssetAmounts {
    base_trade_raw: u64,
    base_gross_raw: u64,
    base_decimals: u8,
    asset_raw: u64,
    asset_decimals: u8,
    fee_raw: u64,
}

pub struct Executor {
    trade_rx: Receiver<ExecCommand>,
    market_tx: Option<TokioSender<MarketCommand>>,
    jupiter: JupiterClient,
    base_mint: Pubkey,
    asset_mint: Pubkey,
    sol_mint: Pubkey,
    base_decimals: u8,
    asset_decimals: u8,
    is_paused: bool,
    pending: HashMap<Signature, PendingSwap>,
    position: Option<PositionState>,
}

impl Executor {
    pub fn new(
        trade_rx: Receiver<ExecCommand>,
        market_tx: Option<TokioSender<MarketCommand>>,
        rpc: Arc<RpcClient>,
        wallet: Arc<solana_sdk::signature::Keypair>,
        base_mint: Pubkey,
        asset_mint: Pubkey,
        slippage_bps: u16,
    ) -> Result<Self> {
        let sol_mint = Pubkey::from_str(SOL_MINT)?;
        let base_decimals = if base_mint == sol_mint {
            9
        } else {
            rpc.get_token_supply(&base_mint)
                .map(|supply| supply.decimals)
                .unwrap_or(0)
        };
        let asset_decimals = rpc
            .get_token_supply(&asset_mint)
            .map(|supply| supply.decimals)
            .unwrap_or(0);

        Ok(Self {
            trade_rx,
            market_tx,
            jupiter: JupiterClient::new(rpc, wallet, slippage_bps),
            base_mint,
            asset_mint,
            sol_mint,
            base_decimals,
            asset_decimals,
            is_paused: false,
            pending: HashMap::new(),
            position: None,
        })
    }

    pub async fn run(mut self) {
        let mut poll = interval(Duration::from_millis(POLL_INTERVAL_MS));
        let mut trade_rx_closed = false;

        loop {
            tokio::select! {
                cmd = self.trade_rx.recv_async(), if !trade_rx_closed => {
                    match cmd {
                        Ok(ExecCommand::Order(order)) => {
                            if self.is_paused {
                                continue;
                            }
                            match order.action {
                                PositionOp::Open => {
                                    if self.position.is_some() || !self.pending.is_empty() {
                                        continue;
                                    }
                                    if let Err(err) = self.submit_open(order).await {
                                        warn!("buy failed: {}", err);
                                    }
                                }
                                PositionOp::Close => {
                                    if self.position.is_none() || !self.pending.is_empty() {
                                        continue;
                                    }
                                    if let Err(err) = self.submit_close().await {
                                        warn!("sell failed: {}", err);
                                        let _ = self
                                            .sync_position_from_chain("submit_close failed")
                                            .await;
                                    }
                                }
                            }
                        }
                        Ok(ExecCommand::Control(ctrl)) => {
                            self.handle_control(ctrl).await;
                        }
                        Err(_) => {
                            warn!("trade channel closed; executor will keep polling");
                            trade_rx_closed = true;
                        }
                    }
                }
                _ = poll.tick() => {
                    if let Err(err) = self.poll_pending().await {
                        warn!("poll pending failed: {}", err);
                    }
                }
            }
        }
    }

    async fn handle_control(&mut self, ctrl: ExecControl) {
        match ctrl {
            ExecControl::Pause => self.is_paused = true,
            ExecControl::Resume => self.is_paused = false,
            ExecControl::ForceClose => {
                if self.position.is_some() && self.pending.is_empty() {
                    if let Err(err) = self.submit_close().await {
                        warn!("force close failed: {}", err);
                        let _ = self.sync_position_from_chain("force_close failed").await;
                    }
                }
            }
            ExecControl::Kill => {
                if self.position.is_some() && self.pending.is_empty() {
                    if let Err(err) = self.submit_close().await {
                        warn!("kill close failed: {}", err);
                        let _ = self.sync_position_from_chain("kill close failed").await;
                    }
                }
            }
        }
    }

    async fn submit_open(&mut self, order: crate::EngineOrder) -> Result<()> {
        if !order.size.is_finite() || order.size <= 0.0 {
            return Err(anyhow!("missing or invalid open order size/notional"));
        }
        let amount = ui_to_amount(order.size, self.base_decimals);
        if amount == 0 {
            return Err(anyhow!("open notional converted to zero raw amount"));
        }
        let submission = self
            .jupiter
            .swap(self.base_mint, self.asset_mint, amount)
            .await?;
        info!("BUY TX: {}", submission.signature);
        self.pending.insert(
            submission.signature,
            PendingSwap {
                intent: PositionOp::Open,
                input_mint: submission.input_mint,
                output_mint: submission.output_mint,
                input_amount: submission.input_amount,
                expected_out_amount: submission.expected_out_amount,
            },
        );
        Ok(())
    }

    async fn submit_close(&mut self) -> Result<()> {
        let amount = token_balance(
            self.jupiter.rpc().as_ref(),
            &self.jupiter.wallet_pubkey(),
            &self.asset_mint,
        )?;
        if amount == 0 {
            return Err(anyhow!("token balance is zero"));
        }
        let submission = self
            .jupiter
            .swap(self.asset_mint, self.base_mint, amount)
            .await?;
        info!("SELL TX: {}", submission.signature);
        self.pending.insert(
            submission.signature,
            PendingSwap {
                intent: PositionOp::Close,
                input_mint: submission.input_mint,
                output_mint: submission.output_mint,
                input_amount: submission.input_amount,
                expected_out_amount: submission.expected_out_amount,
            },
        );
        Ok(())
    }

    async fn poll_pending(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }

        let signatures: Vec<Signature> = self.pending.keys().cloned().collect();
        let statuses = self
            .jupiter
            .rpc()
            .get_signature_statuses(&signatures)?
            .value;

        let mut completed: Vec<Signature> = Vec::new();

        for (sig, status_opt) in signatures.iter().zip(statuses.iter()) {
            let status = match status_opt {
                Some(status) => status,
                None => continue,
            };

            if status.err.is_some() {
                warn!("tx {} failed: {:?}", sig, status.err);
                let _ = self.sync_position_from_chain("tx failed").await;
                completed.push(*sig);
                continue;
            }

            if matches!(
                status.confirmation_status,
                Some(TransactionConfirmationStatus::Processed)
            ) {
                continue;
            }

            let tx = match self
                .jupiter
                .rpc()
                .get_transaction_with_config(sig, Self::tx_config())
            {
                Ok(tx) => tx,
                Err(err) => {
                    warn!("tx {} not available yet: {}", sig, err);
                    continue;
                }
            };

            if let Some(meta) = &tx.transaction.meta {
                if meta.err.is_some() {
                    warn!("tx {} meta error: {:?}", sig, meta.err);
                    let _ = self.sync_position_from_chain("tx meta error").await;
                    completed.push(*sig);
                    continue;
                }
            } else {
                continue;
            }

            let pending = match self.pending.get(sig) {
                Some(pending) => pending.clone(),
                None => continue,
            };

            self.apply_fill(*sig, &pending, &tx).await;
            completed.push(*sig);
        }

        for sig in completed {
            self.pending.remove(&sig);
        }

        Ok(())
    }

    async fn apply_fill(
        &mut self,
        sig: Signature,
        pending: &PendingSwap,
        tx: &EncodedConfirmedTransactionWithStatusMeta,
    ) {
        let fill = self
            .extract_fill(tx, pending)
            .unwrap_or_else(|| self.fallback_fill(pending));

        if let Some(sender) = &self.market_tx {
            let _ = sender
                .send(MarketCommand::SwapFill(SwapFill {
                    signature: sig.to_string(),
                    input_mint: pending.input_mint.to_string(),
                    output_mint: pending.output_mint.to_string(),
                    in_amount: fill.in_amount,
                    out_amount: fill.out_amount,
                }))
                .await;
        }

        match pending.intent {
            PositionOp::Open => {
                let now = get_time_now();
                let Some(amounts) = base_asset_amounts_with_fees(
                    pending,
                    &fill,
                    self.base_mint,
                    self.asset_mint,
                    self.base_mint == self.sol_mint,
                ) else {
                    warn!("open fill did not match base/asset mints");
                    return;
                };
                let asset_size = amount_to_ui(amounts.asset_raw, amounts.asset_decimals);
                let fill_price = price_from_trade_amounts(&amounts).unwrap_or(0.0);

                let position = match &mut self.position {
                    Some(pos) => {
                        if pos.open_tx_hash.is_none() {
                            pos.open_tx_hash = Some(sig.to_string());
                        }
                        pos.apply_open_fill(
                            asset_size,
                            fill_price,
                            amounts.base_trade_raw,
                            amounts.asset_raw,
                            amounts.fee_raw,
                        );
                        pos
                    }
                    None => {
                        self.position = Some(PositionState::new(
                            now,
                            Some(sig.to_string()),
                            asset_size,
                            fill_price,
                            amounts.base_trade_raw,
                            amounts.asset_raw,
                        ));
                        self.position.as_mut().unwrap()
                    }
                };

                let open_pos = position.open_pos_info();
                if let Some(sender) = &self.market_tx {
                    let _ = sender
                        .send(MarketCommand::OpenPosition(Some(open_pos)))
                        .await;
                }
                info!(
                    "position opened/added at {}ms; entry_px {:.8}, size {:.8}",
                    position.open_time, position.entry_px, position.size
                );
            }
            PositionOp::Close => {
                let now = get_time_now();
                let Some(amounts) = base_asset_amounts_with_fees(
                    pending,
                    &fill,
                    self.base_mint,
                    self.asset_mint,
                    self.base_mint == self.sol_mint,
                ) else {
                    warn!("close fill did not match base/asset mints");
                    return;
                };
                let asset_size = amount_to_ui(amounts.asset_raw, amounts.asset_decimals);
                let close_price = price_from_trade_amounts(&amounts).unwrap_or(0.0);

                let fully_closed = {
                    let Some(position) = &mut self.position else {
                        warn!("close fill received but no open position tracked");
                        return;
                    };

                    position.apply_close_fill(
                        asset_size,
                        amounts.base_trade_raw,
                        amounts.base_gross_raw,
                        amounts.asset_raw,
                        amounts.fee_raw,
                    )
                };

                if fully_closed {
                    let raw_balance = match self.asset_balance_raw() {
                        Ok(raw) => raw,
                        Err(err) => {
                            warn!("failed to refresh balance after close: {}", err);
                            self.finalize_closed_position(
                                now,
                                close_price,
                                sig.to_string(),
                                "balance refresh failed after full close",
                            )
                            .await;
                            return;
                        }
                    };

                    if raw_balance > MIN_ASSET_DUST_RAW {
                        let Some(position) = &mut self.position else {
                            warn!("position missing while reconciling balance");
                            return;
                        };
                        position.size = amount_to_ui(raw_balance, self.asset_decimals);
                        position.open_asset_raw = raw_balance;
                        let open_pos = position.open_pos_info();
                        let remaining_size = position.size;
                        if let Some(sender) = &self.market_tx {
                            let _ = sender
                                .send(MarketCommand::OpenPosition(Some(open_pos)))
                                .await;
                        }
                        info!(
                            "close fill but residual balance remains; size {:.8}",
                            remaining_size
                        );
                        return;
                    }
                    self.finalize_closed_position(now, close_price, sig.to_string(), "full close")
                        .await;
                } else {
                    let Some(position) = &self.position else {
                        warn!("position missing while reporting partial close");
                        return;
                    };
                    let open_pos = position.open_pos_info();
                    let remaining_size = position.size;
                    if let Some(sender) = &self.market_tx {
                        let _ = sender
                            .send(MarketCommand::OpenPosition(Some(open_pos)))
                            .await;
                    }
                    info!(
                        "position partially closed; remaining size {:.8}",
                        remaining_size
                    );
                }
            }
        }
    }

    async fn sync_position_from_chain(&mut self, reason: &str) -> Result<()> {
        let raw_balance = self.asset_balance_raw()?;

        if raw_balance <= MIN_ASSET_DUST_RAW {
            if self.position.is_some() {
                info!("position closed on-chain ({reason})");
            }
            self.finalize_closed_position(
                get_time_now(),
                0.0,
                format!("sync:{reason}"),
                "sync close",
            )
            .await;
            return Ok(());
        }

        let size = amount_to_ui(raw_balance, self.asset_decimals);
        let now = get_time_now();
        let position = self.position.get_or_insert_with(|| {
            warn!("position desync detected; rebuilding from chain ({reason})");
            PositionState::new(now, None, size, 0.0, 0, raw_balance)
        });
        position.size = size;
        position.open_asset_raw = raw_balance;

        if let Some(sender) = &self.market_tx {
            let _ = sender
                .send(MarketCommand::OpenPosition(Some(position.open_pos_info())))
                .await;
        }
        Ok(())
    }

    fn asset_balance_raw(&self) -> Result<u64> {
        match token_balance(
            self.jupiter.rpc().as_ref(),
            &self.jupiter.wallet_pubkey(),
            &self.asset_mint,
        ) {
            Ok(raw) => Ok(raw),
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("token account not found") || msg.contains("zero balance") {
                    Ok(0)
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn finalize_closed_position(
        &mut self,
        close_time: u64,
        close_price_hint: f64,
        close_tx_hash: String,
        reason: &str,
    ) {
        let Some(position) = self.position.take() else {
            if let Some(sender) = &self.market_tx {
                let _ = sender.send(MarketCommand::OpenPosition(None)).await;
            }
            return;
        };

        // Emit a realized trade only when we have close leg data.
        let has_close_data = position.close_asset_raw > 0 || position.close_base_trade_raw > 0;
        if has_close_data {
            let open_fill = position.open_fill_info(self.base_decimals, self.asset_decimals);
            let close_fill = FillInfo {
                time: close_time,
                price: if close_price_hint > 0.0 {
                    close_price_hint
                } else {
                    position.close_price(self.base_decimals, self.asset_decimals)
                },
                side: TradeSide::Sell,
                in_amount: position.close_asset_raw,
                out_amount: position.close_base_trade_raw,
            };

            let pnl = position.pnl(self.base_decimals);
            let fees = amount_to_ui(position.fees_raw, self.base_decimals);
            let trade = TradeInfo {
                open_tx_hash: position.open_tx_hash.clone().unwrap_or_default(),
                close_tx_hash,
                side: TradeSide::Buy,
                size: position.open_size(self.asset_decimals),
                pnl,
                fees,
                funding: 0.0,
                open: open_fill,
                close: close_fill,
            };
            if let Some(sender) = &self.market_tx {
                let _ = sender.send(MarketCommand::Trade(trade)).await;
            }
            info!("position closed ({reason})");
        } else {
            warn!("position closed without close fill data ({reason}); skipping Trade event emit");
        }

        if let Some(sender) = &self.market_tx {
            let _ = sender.send(MarketCommand::OpenPosition(None)).await;
        }
    }

    fn extract_fill(
        &self,
        tx: &EncodedConfirmedTransactionWithStatusMeta,
        pending: &PendingSwap,
    ) -> Option<SwapFillAmounts> {
        let meta = tx.transaction.meta.as_ref()?;
        let fee_lamports = meta.fee;
        let sol_wallet_delta = lamport_delta(meta, tx, &self.jupiter.wallet_pubkey());

        let (in_amount, in_decimals) = if pending.input_mint == self.sol_mint {
            let lamports = sol_wallet_delta?;
            (abs_i128(lamports), 9)
        } else {
            let (delta, decimals) = token_balance_delta(
                &meta.pre_token_balances,
                &meta.post_token_balances,
                &pending.input_mint,
                &self.jupiter.wallet_pubkey(),
            )?;
            (abs_i128(delta), decimals)
        };

        let (out_amount, out_decimals) = if pending.output_mint == self.sol_mint {
            let lamports = sol_wallet_delta?;
            (abs_i128(lamports), 9)
        } else {
            let (delta, decimals) = token_balance_delta(
                &meta.pre_token_balances,
                &meta.post_token_balances,
                &pending.output_mint,
                &self.jupiter.wallet_pubkey(),
            )?;
            (abs_i128(delta), decimals)
        };

        Some(SwapFillAmounts {
            in_amount,
            out_amount,
            in_decimals,
            out_decimals,
            fee_lamports,
            sol_wallet_delta,
        })
    }

    fn fallback_fill(&self, pending: &PendingSwap) -> SwapFillAmounts {
        let in_decimals = self.decimals_for_mint(pending.input_mint);
        let out_decimals = self.decimals_for_mint(pending.output_mint);

        SwapFillAmounts {
            in_amount: pending.input_amount,
            out_amount: pending.expected_out_amount.unwrap_or(0),
            in_decimals,
            out_decimals,
            fee_lamports: 0,
            sol_wallet_delta: None,
        }
    }

    fn decimals_for_mint(&self, mint: Pubkey) -> u8 {
        if mint == self.sol_mint {
            9
        } else if mint == self.base_mint {
            self.base_decimals
        } else if mint == self.asset_mint {
            self.asset_decimals
        } else {
            0
        }
    }

    fn tx_config() -> RpcTransactionConfig {
        RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::JsonParsed),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        }
    }
}

fn amount_to_ui(amount: u64, decimals: u8) -> f64 {
    if decimals == 0 {
        return amount as f64;
    }
    let divisor = 10_f64.powi(decimals as i32);
    amount as f64 / divisor
}

fn ui_to_amount(value: f64, decimals: u8) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let multiplier = 10_f64.powi(decimals as i32);
    let raw = (value * multiplier).floor();
    if raw <= 0.0 {
        0
    } else if raw >= u64::MAX as f64 {
        u64::MAX
    } else {
        raw as u64
    }
}

fn price_from_trade_amounts(amounts: &BaseAssetAmounts) -> Option<f64> {
    let base_ui = amount_to_ui(amounts.base_trade_raw, amounts.base_decimals);
    let asset_ui = amount_to_ui(amounts.asset_raw, amounts.asset_decimals);
    if asset_ui == 0.0 {
        return None;
    }
    Some(base_ui / asset_ui)
}

fn base_asset_amounts_with_fees(
    pending: &PendingSwap,
    fill: &SwapFillAmounts,
    base_mint: Pubkey,
    asset_mint: Pubkey,
    base_is_sol: bool,
) -> Option<BaseAssetAmounts> {
    let is_buy = pending.input_mint == base_mint && pending.output_mint == asset_mint;
    let is_sell = pending.output_mint == base_mint && pending.input_mint == asset_mint;

    if is_buy {
        let asset_raw = fill.out_amount;
        let asset_decimals = fill.out_decimals;
        let base_decimals = fill.in_decimals;

        if base_is_sol {
            let actual_spent = fill
                .sol_wallet_delta
                .map(abs_i128)
                .unwrap_or(fill.in_amount);
            let intended = pending.input_amount;
            let fee_raw = actual_spent.saturating_sub(intended);
            let base_trade_raw = intended;
            let base_gross_raw = intended.saturating_add(fee_raw);
            return Some(BaseAssetAmounts {
                base_trade_raw,
                base_gross_raw,
                base_decimals,
                asset_raw,
                asset_decimals,
                fee_raw,
            });
        }

        return Some(BaseAssetAmounts {
            base_trade_raw: fill.in_amount,
            base_gross_raw: fill.in_amount,
            base_decimals,
            asset_raw,
            asset_decimals,
            fee_raw: 0,
        });
    }

    if is_sell {
        let asset_raw = fill.in_amount;
        let asset_decimals = fill.in_decimals;
        let base_decimals = fill.out_decimals;

        if base_is_sol {
            let net_received = fill
                .sol_wallet_delta
                .map(abs_i128)
                .unwrap_or(fill.out_amount);
            let fee_raw = fill.fee_lamports;
            let base_trade_raw = net_received;
            let base_gross_raw = net_received.saturating_add(fee_raw);
            return Some(BaseAssetAmounts {
                base_trade_raw,
                base_gross_raw,
                base_decimals,
                asset_raw,
                asset_decimals,
                fee_raw,
            });
        }

        return Some(BaseAssetAmounts {
            base_trade_raw: fill.out_amount,
            base_gross_raw: fill.out_amount,
            base_decimals,
            asset_raw,
            asset_decimals,
            fee_raw: 0,
        });
    }

    None
}

fn token_balance_delta(
    pre: &OptionSerializer<Vec<UiTransactionTokenBalance>>,
    post: &OptionSerializer<Vec<UiTransactionTokenBalance>>,
    mint: &Pubkey,
    owner: &Pubkey,
) -> Option<(i128, u8)> {
    let (pre_total, pre_decimals) = token_balance_total(pre, mint, owner);
    let (post_total, post_decimals) = token_balance_total(post, mint, owner);
    let decimals = post_decimals.or(pre_decimals)?;
    Some((post_total as i128 - pre_total as i128, decimals))
}

fn token_balance_total(
    balances: &OptionSerializer<Vec<UiTransactionTokenBalance>>,
    mint: &Pubkey,
    owner: &Pubkey,
) -> (u64, Option<u8>) {
    let mut total = 0_u64;
    let mut decimals = None;
    let mint_str = mint.to_string();
    let owner_str = owner.to_string();

    let entries = match balances {
        OptionSerializer::Some(list) => list,
        _ => return (0, None),
    };

    for entry in entries {
        if entry.mint != mint_str {
            continue;
        }
        let owner_matches = match &entry.owner {
            OptionSerializer::Some(o) => o == &owner_str,
            _ => false,
        };
        if !owner_matches {
            continue;
        }
        let amount = entry.ui_token_amount.amount.parse::<u64>().unwrap_or(0);
        total = total.saturating_add(amount);
        decimals = Some(entry.ui_token_amount.decimals);
    }

    (total, decimals)
}

fn lamport_delta(
    meta: &UiTransactionStatusMeta,
    tx: &EncodedConfirmedTransactionWithStatusMeta,
    wallet: &Pubkey,
) -> Option<i128> {
    let idx = wallet_account_index(tx, wallet)?;
    let pre = *meta.pre_balances.get(idx)? as i128;
    let post = *meta.post_balances.get(idx)? as i128;
    Some(post - pre)
}

fn wallet_account_index(
    tx: &EncodedConfirmedTransactionWithStatusMeta,
    wallet: &Pubkey,
) -> Option<usize> {
    let wallet_str = wallet.to_string();
    match &tx.transaction.transaction {
        EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            UiMessage::Parsed(msg) => msg.account_keys.iter().position(|k| k.pubkey == wallet_str),
            UiMessage::Raw(msg) => msg.account_keys.iter().position(|k| k == &wallet_str),
        },
        EncodedTransaction::Accounts(list) => list
            .account_keys
            .iter()
            .position(|k| k.pubkey == wallet_str),
        _ => None,
    }
}

fn abs_i128(value: i128) -> u64 {
    if value < 0 {
        (-value) as u64
    } else {
        value as u64
    }
}
