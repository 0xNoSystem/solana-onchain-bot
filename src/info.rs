use anyhow::{Result, anyhow};
use kwant::Price;
use serde_json::Value;
use solana_account_decoder_client_types::{UiAccount, UiAccountData, UiAccountEncoding};
use solana_client::nonblocking::pubsub_client::PubsubClient;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcProgramAccountsConfig;
use solana_client::rpc_filter::{Memcmp, RpcFilterType};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_rpc_client_types::config::RpcAccountInfoConfig;
use solana_rpc_client_types::response::Response;
use std::collections::HashMap;
use std::env;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::oneshot;
use tokio_stream::StreamExt;

use crate::helpers::get_time_now;

const MAINNET_RPC: &str = "https://api.mainnet-beta.solana.com";
const MAINNET_WSS: &str = "wss://api.mainnet-beta.solana.com";

const PUMPSWAP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

const OFF_BASE_MINT: usize = 43;
const OFF_QUOTE_MINT: usize = 75;
const OFF_BASE_VAULT: usize = 139;
const OFF_QUOTE_VAULT: usize = 171;

const ONE_MINUTE_MS: u64 = 60_000;

pub struct TokenInfo {
    pub mint: Pubkey,
    pub pool_state: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    stop: oneshot::Sender<()>,
}

pub struct StreamManager {
    rpc: RpcClient,
    rpc_url: String,
    ws_url: String,
    subscriptions: HashMap<u32, TokenInfo>,
    next_sub_id: u32,
}

impl StreamManager {
    pub fn new() -> Result<Self> {
        dotenv::dotenv().ok();

        let rpc_url = env::var("RPC_URL").unwrap_or_else(|_| MAINNET_RPC.to_string());
        let ws_url = env::var("WS_URL").unwrap_or_else(|_| MAINNET_WSS.to_string());

        Ok(Self {
            rpc: RpcClient::new(rpc_url.clone()),
            rpc_url,
            ws_url,
            subscriptions: HashMap::new(),
            next_sub_id: 1,
        })
    }

    pub async fn subscribe_candles(
        &mut self,
        mint: Pubkey,
    ) -> Result<(u32, UnboundedReceiver<Price>)> {
        let (pool_state, base_vault, quote_vault) = self.resolve_pool_and_vaults(&mint).await?;

        let (tx, rx) = unbounded_channel::<Price>();
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let ws_url = self.ws_url.clone();
        let rpc_url = self.rpc_url.clone();

        tokio::spawn(async move {
            let rpc = RpcClient::new(rpc_url);
            let mut clock = SlotTimeEstimator::new(rpc, Duration::from_secs(45));
            let ws = match PubsubClient::new(ws_url).await {
                Ok(client) => client,
                Err(_) => return,
            };

            let config = RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::JsonParsed),
                commitment: Some(CommitmentConfig::confirmed()),
                data_slice: None,
                min_context_slot: None,
            };

            let (mut base_stream, _base_unsub) = match ws
                .account_subscribe(&base_vault, Some(config.clone()))
                .await
            {
                Ok(stream) => stream,
                Err(_) => return,
            };
            let (mut quote_stream, _quote_unsub) =
                match ws.account_subscribe(&quote_vault, Some(config)).await {
                    Ok(stream) => stream,
                    Err(_) => return,
                };

            let mut balances: HashMap<&'static str, (f64, u8)> = HashMap::new();
            let mut current_bucket: Option<u64> = None;
            let mut current_candle: Option<Price> = None;
            let mut last_close: Option<f64> = None;

            'stream: loop {
                let slot = tokio::select! {
                    _ = &mut stop_rx => {
                        break 'stream;
                    }
                    Some(msg) = base_stream.next() => {
                        let s = msg.context.slot;
                        clock.maybe_refresh(s).await;
                        if let Ok((amount, decimals)) = parse_token_amount(&msg) {
                            balances.insert("base", (amount, decimals));
                        }
                        s
                    }
                    Some(msg) = quote_stream.next() => {
                        let s = msg.context.slot;
                        clock.maybe_refresh(s).await;
                        if let Ok((amount, decimals)) = parse_token_amount(&msg) {
                            balances.insert("quote", (amount, decimals));
                        }
                        s
                    }
                    else => break 'stream,
                };

                let (Some((base_amt, base_dec)), Some((quote_amt, quote_dec))) =
                    (balances.get("base"), balances.get("quote"))
                else {
                    continue;
                };

                let base = base_amt / 10f64.powi(*base_dec as i32);
                let quote = quote_amt / 10f64.powi(*quote_dec as i32);
                if base <= 0.0 {
                    continue;
                }

                let price = quote / base;
                let now = clock.estimate_ms(slot).unwrap_or_else(get_time_now);
                let bucket = now / ONE_MINUTE_MS;
                let open_time = bucket * ONE_MINUTE_MS;
                let close_time = open_time + 59_999;

                match current_bucket {
                    None => {
                        current_bucket = Some(bucket);
                        current_candle = Some(Price {
                            open: price,
                            high: price,
                            low: price,
                            close: price,
                            open_time,
                            close_time,
                            vlm: 0.0,
                        });
                        if let Some(candle) = current_candle.as_ref() {
                            if tx.send(*candle).is_err() {
                                break;
                            }
                        }
                    }
                    Some(active) if active == bucket => {
                        if let Some(candle) = current_candle.as_mut() {
                            if price > candle.high {
                                candle.high = price;
                            }
                            if price < candle.low {
                                candle.low = price;
                            }
                            candle.close = price;
                            if tx.send(*candle).is_err() {
                                break;
                            }
                        }
                    }
                    Some(active) if bucket > active => {
                        if let Some(prev) = current_candle.take() {
                            last_close = Some(prev.close);
                            if tx.send(prev).is_err() {
                                break;
                            }
                        }

                        if let Some(last) = last_close {
                            for gap in (active + 1)..bucket {
                                let gap_open = gap * ONE_MINUTE_MS;
                                let gap_close = gap_open + 59_999;
                                let flat = Price {
                                    open: last,
                                    high: last,
                                    low: last,
                                    close: last,
                                    open_time: gap_open,
                                    close_time: gap_close,
                                    vlm: 0.0,
                                };
                                if tx.send(flat).is_err() {
                                    return;
                                }
                            }
                        }

                        current_bucket = Some(bucket);
                        current_candle = Some(Price {
                            open: price,
                            high: price,
                            low: price,
                            close: price,
                            open_time,
                            close_time,
                            vlm: 0.0,
                        });
                        if let Some(candle) = current_candle.as_ref() {
                            if tx.send(*candle).is_err() {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        let sub_id = self.next_sub_id;
        self.next_sub_id = self.next_sub_id.wrapping_add(1);
        self.subscriptions.insert(
            sub_id,
            TokenInfo {
                mint,
                pool_state,
                base_vault,
                quote_vault,
                stop: stop_tx,
            },
        );

        Ok((sub_id, rx))
    }

    pub fn unsubscribe_candles(&mut self, sub_id: u32) -> Result<()> {
        let info = self
            .subscriptions
            .remove(&sub_id)
            .ok_or_else(|| anyhow!("unknown subscription: {sub_id}"))?;
        let _ = info.stop.send(());
        Ok(())
    }

    async fn resolve_pool_and_vaults(&self, mint: &Pubkey) -> Result<(Pubkey, Pubkey, Pubkey)> {
        let program = Pubkey::from_str(PUMPSWAP_PROGRAM)?;
        let wsol = Pubkey::from_str(WSOL_MINT)?;

        let cfg = RpcProgramAccountsConfig {
            filters: Some(vec![
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(OFF_BASE_MINT, mint.as_ref())),
                RpcFilterType::Memcmp(Memcmp::new_base58_encoded(OFF_QUOTE_MINT, wsol.as_ref())),
            ]),
            account_config: solana_client::rpc_config::RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                data_slice: None,
                commitment: Some(CommitmentConfig::confirmed()),
                min_context_slot: None,
            },
            with_context: None,
            sort_results: None,
        };

        let accounts = self
            .rpc
            .get_program_ui_accounts_with_config(&program, cfg)
            .await?;
        let (pool_state, ui_account) = accounts
            .get(0)
            .ok_or_else(|| anyhow!("no pool account found"))?;

        let bytes = ui_account
            .data
            .decode()
            .ok_or_else(|| anyhow!("failed to decode pool account"))?;
        let (base_vault, quote_vault) = extract_vaults(&bytes)?;

        Ok((*pool_state, base_vault, quote_vault))
    }
}

fn parse_token_amount(msg: &Response<UiAccount>) -> Result<(f64, u8)> {
    let UiAccountData::Json(parsed) = &msg.value.data else {
        return Err(anyhow!("expected JsonParsed token account"));
    };

    let info = parsed
        .parsed
        .get("info")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing info"))?;

    let token_amount = info
        .get("tokenAmount")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing tokenAmount"))?;

    let amount: f64 = token_amount
        .get("amount")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing amount"))?
        .parse()?;

    let decimals: u8 = token_amount
        .get("decimals")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("missing decimals"))? as u8;

    Ok((amount, decimals))
}

fn extract_vaults(data: &[u8]) -> Result<(Pubkey, Pubkey)> {
    if data.len() < OFF_QUOTE_VAULT + 32 {
        return Err(anyhow!(
            "pool account too short: {} bytes, expected at least {}",
            data.len(),
            OFF_QUOTE_VAULT + 32
        ));
    }

    let base_bytes: [u8; 32] = data[OFF_BASE_VAULT..OFF_BASE_VAULT + 32]
        .try_into()
        .map_err(|_| anyhow!("invalid base vault bytes"))?;
    let quote_bytes: [u8; 32] = data[OFF_QUOTE_VAULT..OFF_QUOTE_VAULT + 32]
        .try_into()
        .map_err(|_| anyhow!("invalid quote vault bytes"))?;

    Ok((
        Pubkey::new_from_array(base_bytes),
        Pubkey::new_from_array(quote_bytes),
    ))
}

#[derive(Clone, Copy)]
struct Anchor {
    slot: u64,
    time_ms: u64,
}

struct SlotTimeEstimator {
    rpc: RpcClient,
    anchor: Option<Anchor>,
    prev_anchor: Option<Anchor>,
    ms_per_slot: f64,
    last_refresh: Instant,
    refresh_interval: Duration,
}

impl SlotTimeEstimator {
    fn new(rpc: RpcClient, refresh_interval: Duration) -> Self {
        Self {
            rpc,
            anchor: None,
            prev_anchor: None,
            ms_per_slot: 400.0,
            last_refresh: Instant::now() - refresh_interval,
            refresh_interval,
        }
    }

    async fn maybe_refresh(&mut self, slot: u64) {
        if self.last_refresh.elapsed() < self.refresh_interval {
            return;
        }

        let Ok(ts) = self.rpc.get_block_time(slot).await else {
            return;
        };

        if ts <= 0 {
            return;
        }

        let time_ms = (ts as u64) * 1000;
        self.prev_anchor = self.anchor.take();
        self.anchor = Some(Anchor { slot, time_ms });

        if let Some(prev) = self.prev_anchor {
            if slot > prev.slot && time_ms > prev.time_ms {
                let delta_ms = (time_ms - prev.time_ms) as f64;
                let delta_slots = (slot - prev.slot) as f64;
                let mps = delta_ms / delta_slots;
                if (200.0..=1000.0).contains(&mps) {
                    self.ms_per_slot = mps;
                }
            }
        }

        self.last_refresh = Instant::now();
    }

    fn estimate_ms(&self, slot: u64) -> Option<u64> {
        let anchor = self.anchor?;
        if slot >= anchor.slot {
            let diff = (slot - anchor.slot) as f64 * self.ms_per_slot;
            Some(anchor.time_ms + diff.round() as u64)
        } else {
            let diff = (anchor.slot - slot) as f64 * self.ms_per_slot;
            anchor.time_ms.checked_sub(diff.round() as u64)
        }
    }
}
