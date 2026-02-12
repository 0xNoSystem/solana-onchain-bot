use anyhow::{Result, anyhow};
use kwant::Price;
use log::warn;
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::{TimeFrame, get_time_now_and_candles_ago};

const BASE_URL: &str = "https://public-api.birdeye.so/defi/ohlcv";
const DEFAULT_MIN_INTERVAL_MS: u64 = 500;
const DEFAULT_MAX_RETRIES: u32 = 6;
const DEFAULT_BACKOFF_BASE_MS: u64 = 700;
const DEFAULT_BACKOFF_MAX_MS: u64 = 10_000;
const MAX_BACKOFF_SHIFT: u32 = 16;

#[derive(Debug, Clone, Copy)]
pub enum QuoteCurrency {
    Sol,
    Usd,
}

impl QuoteCurrency {
    fn as_str(&self) -> &'static str {
        match self {
            QuoteCurrency::Sol => "native",
            QuoteCurrency::Usd => "usd",
        }
    }
}

#[derive(Debug, Deserialize)]
struct BirdeyeResponse {
    data: Option<BirdeyeData>,
    success: bool,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BirdeyeData {
    items: Vec<BirdeyeCandle>,
}

#[derive(Debug, Deserialize)]
struct BirdeyeCandle {
    o: f64,
    h: f64,
    l: f64,
    c: f64,
    v: f64,
    #[serde(rename = "unixTime")]
    unix_time: u64,
}

pub struct BirdeyeClient {
    client: reqwest::Client,
    api_key: String,
    cfg: BirdeyeRateLimitConfig,
}

#[derive(Clone, Copy, Debug)]
struct BirdeyeRateLimitConfig {
    min_interval_ms: u64,
    max_retries: u32,
    backoff_base_ms: u64,
    backoff_max_ms: u64,
}

impl BirdeyeRateLimitConfig {
    fn from_env() -> Self {
        let min_interval_ms = env_u64("BIRDEYE_MIN_INTERVAL_MS", DEFAULT_MIN_INTERVAL_MS);
        let max_retries = env_u64("BIRDEYE_MAX_RETRIES", DEFAULT_MAX_RETRIES as u64) as u32;
        let backoff_base_ms = env_u64("BIRDEYE_BACKOFF_BASE_MS", DEFAULT_BACKOFF_BASE_MS);
        let backoff_max_ms = env_u64("BIRDEYE_BACKOFF_MAX_MS", DEFAULT_BACKOFF_MAX_MS);
        let backoff_max_ms = backoff_max_ms.max(backoff_base_ms);

        Self {
            min_interval_ms,
            max_retries,
            backoff_base_ms,
            backoff_max_ms,
        }
    }
}

static BIRDEYE_GLOBAL_PACER: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

impl BirdeyeClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            cfg: BirdeyeRateLimitConfig::from_env(),
        }
    }

    pub async fn fetch_candles(
        &self,
        token: &str,
        tf: TimeFrame,
        currency: QuoteCurrency,
        candle_count: u64,
    ) -> Result<Vec<Price>> {
        let (time_from, time_to) = get_time_now_and_candles_ago(candle_count, tf);

        let interval_str = birdeye_interval(tf);
        let interval_ms = tf.to_millis();

        let url = format!(
            "{BASE_URL}?address={token}&type={interval_str}&currency={currency}&time_from={from}&time_to={to}&ui_amount_mode=raw",
            currency = currency.as_str(),
            to = time_to / 1000,
            from = time_from / 1000,
        );

        for attempt in 0..=self.cfg.max_retries {
            wait_for_birdeye_slot(self.cfg.min_interval_ms).await;

            let send_res = self
                .client
                .get(&url)
                .header("X-API-KEY", &self.api_key)
                .header("accept", "application/json")
                .header("x-chain", "solana")
                .send()
                .await;

            let response = match send_res {
                Ok(response) => response,
                Err(err) => {
                    if attempt < self.cfg.max_retries {
                        let delay = retry_delay(self.cfg, attempt, None);
                        warn!(
                            "birdeye request error for token={} tf={} attempt={}/{}: {} (retry in {}ms)",
                            token,
                            interval_str,
                            attempt + 1,
                            self.cfg.max_retries + 1,
                            err,
                            delay.as_millis()
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(anyhow!(
                        "Birdeye request failed for token={} tf={} after {} attempts: {}",
                        token,
                        interval_str,
                        self.cfg.max_retries + 1,
                        err
                    ));
                }
            };

            let status = response.status();
            let retry_after = parse_retry_after(response.headers());
            let body = response.text().await?;

            if !status.is_success() {
                if is_retryable_status(status) && attempt < self.cfg.max_retries {
                    let delay = retry_delay(self.cfg, attempt, retry_after);
                    warn!(
                        "birdeye HTTP {} for token={} tf={} attempt={}/{} (retry in {}ms)",
                        status,
                        token,
                        interval_str,
                        attempt + 1,
                        self.cfg.max_retries + 1,
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(anyhow!(
                    "Birdeye HTTP {} for token={} tf={}: {}",
                    status,
                    token,
                    interval_str,
                    body
                ));
            }

            let parsed: BirdeyeResponse = serde_json::from_str(&body)
                .map_err(|err| anyhow!("Birdeye parse error: {err}; body: {body}"))?;

            if !parsed.success {
                let message = parsed
                    .message
                    .unwrap_or_else(|| "unknown error".to_string());
                if is_rate_limit_message(&message) && attempt < self.cfg.max_retries {
                    let delay = retry_delay(self.cfg, attempt, retry_after);
                    warn!(
                        "birdeye rate limited for token={} tf={} attempt={}/{}: {} (retry in {}ms)",
                        token,
                        interval_str,
                        attempt + 1,
                        self.cfg.max_retries + 1,
                        message,
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(anyhow!(
                    "Birdeye API returned success=false for token={} tf={}: {}",
                    token,
                    interval_str,
                    message
                ));
            }

            let data = parsed
                .data
                .ok_or_else(|| anyhow!("Birdeye API response missing data field"))?;

            let mut candles: Vec<Price> = data
                .items
                .into_iter()
                .map(|c| {
                    let open_ms = c.unix_time * 1000;

                    Price {
                        open: c.o,
                        high: c.h,
                        low: c.l,
                        close: c.c,
                        open_time: open_ms,
                        close_time: open_ms + interval_ms,
                        vlm: c.v,
                    }
                })
                .collect();

            candles.sort_by_key(|c| c.open_time);
            return Ok(fill_gaps(candles, interval_ms));
        }

        Err(anyhow!(
            "Birdeye retries exhausted for token={} tf={}",
            token,
            interval_str
        ))
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

async fn wait_for_birdeye_slot(min_interval_ms: u64) {
    if min_interval_ms == 0 {
        return;
    }

    let gate = BIRDEYE_GLOBAL_PACER.get_or_init(|| Mutex::new(None));
    let mut last = gate.lock().await;
    let min_interval = Duration::from_millis(min_interval_ms);

    if let Some(prev) = *last {
        let elapsed = prev.elapsed();
        if elapsed < min_interval {
            tokio::time::sleep(min_interval - elapsed).await;
        }
    }

    *last = Some(Instant::now());
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn is_rate_limit_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("too many requests") || msg.contains("rate limit")
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let raw = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())?;
    let seconds = raw.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

fn retry_delay(
    cfg: BirdeyeRateLimitConfig,
    attempt: u32,
    retry_after: Option<Duration>,
) -> Duration {
    let shift = attempt.min(MAX_BACKOFF_SHIFT);
    let factor = 1_u64 << shift;
    let exp_ms = cfg
        .backoff_base_ms
        .saturating_mul(factor)
        .min(cfg.backoff_max_ms);

    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        % 250;

    let base = Duration::from_millis(exp_ms.saturating_add(jitter_ms));
    match retry_after {
        Some(wait) if wait > base => wait,
        _ => base,
    }
}

fn birdeye_interval(tf: TimeFrame) -> &'static str {
    match tf {
        TimeFrame::Min1 => "1m",
        TimeFrame::Min3 => "3m",
        TimeFrame::Min5 => "5m",
        TimeFrame::Min15 => "15m",
        TimeFrame::Min30 => "30m",
        TimeFrame::Hour1 => "1H",
        TimeFrame::Hour2 => "2H",
        TimeFrame::Hour4 => "4H",
        TimeFrame::Hour12 => "12H",
        TimeFrame::Day1 => "1D",
        TimeFrame::Day3 => "3D",
        TimeFrame::Week => "1W",
        TimeFrame::Month => "1M",
    }
}

fn fill_gaps(candles: Vec<Price>, interval_ms: u64) -> Vec<Price> {
    if candles.is_empty() {
        return candles;
    }

    let mut out = Vec::with_capacity(candles.len());

    for i in 0..candles.len() - 1 {
        let current = candles[i];
        let next = candles[i + 1];

        out.push(current);

        let mut expected = current.open_time + interval_ms;

        while expected < next.open_time {
            out.push(Price {
                open: current.close,
                high: current.close,
                low: current.close,
                close: current.close,
                open_time: expected,
                close_time: expected + interval_ms,
                vlm: 0.0,
            });

            expected += interval_ms;
        }
    }

    out.push(*candles.last().unwrap());
    out
}
