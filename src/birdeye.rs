use anyhow::{Result, anyhow};
use kwant::Price;
use serde::Deserialize;

use crate::{TimeFrame, get_time_now_and_candles_ago};

const BASE_URL: &str = "https://public-api.birdeye.so/defi/ohlcv";

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
    data: BirdeyeData,
    success: bool,
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
}

impl BirdeyeClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
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

        let interval_str = tf.as_str();
        let interval_ms = tf.to_millis();

        let url = format!(
            "{BASE_URL}?address={token}&type={interval_str}&currency={currency}&time_from={from}&time_to={to}&ui_amount_mode=raw",
            currency = currency.as_str(),
            to = time_to / 1000,
            from = time_from / 1000,
        );

        let resp = self
            .client
            .get(url)
            .header("X-API-KEY", &self.api_key)
            .header("accept", "application/json")
            .header("x-chain", "solana")
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;

        if !status.is_success() {
            return Err(anyhow!("Birdeye HTTP {status}: {body}"));
        }

        let resp: BirdeyeResponse = serde_json::from_str(&body)
            .map_err(|err| anyhow!("Birdeye parse error: {err}; body: {body}"))?;

        if !resp.success {
            return Err(anyhow!("Birdeye API returned success=false; body: {body}"));
        }

        let mut candles: Vec<Price> = resp
            .data
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

        Ok(fill_gaps(candles, interval_ms))
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
